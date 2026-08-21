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
Milestone 4 work; optional media remains stable-v1 work as specified by the plan.
Periodic dynamic-category reconciliation was completed after this gate.

## Milestone 4 progress

The daemon foundation, scheduling, service-durability, socket-recovery, and offline-
diagnostics checkpoints are implemented and validated:

- a versioned, bounded local Unix-socket contract with private library-local daemon
  and cooperative writer-lease sockets;
- race-safe writer discovery for GUI/CLI callers, coordinated recovery of confirmed
  stale sockets, and fail-closed handling of live or unexpected socket paths;
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

Scheduling now includes:

- schema migration 7 with durable per-collection manual, bounded interval, and daily-
  UTC schedules; pause state; bounded jitter; next-run and last-started cursors; and
  atomic compare-and-swap claims that prevent duplicate starts;
- deterministic delay-only jitter and restart/sleep recovery that coalesces any
  number of missed occurrences into one resumable synchronization before advancing
  directly to a future occurrence;
- daemon background dispatch of at most one claimed schedule at a time through the
  existing reconciliation path, retaining the single-writer boundary;
- GUI controls for schedules during collection creation and for later cadence,
  jitter, and pause edits, whether the GUI holds the direct writer lease or forwards
  to the daemon; and
- cooperative `SIGTERM`/`SIGINT` handling plus a real-daemon restart test proving an
  overdue occurrence runs once and is not duplicated after restart.

Service durability and diagnostics now include:

- cancellation-aware long-gap reconciliation with checks at bounded job, request,
  batch, revision, page-head, search, and checkpoint boundaries; already durable
  canonical revisions are retained while the active job/run remains resumable;
- bounded retry attempts, capped equal-jitter exponential backoff, bounded
  `Retry-After` handling, and a shared in-process circuit breaker for retryable
  MediaWiki failures;
- durable discovery of unfinished collection reconciliations before later schedule
  occurrences, plus a real-daemon throttle/partial-failure/restart gate proving that
  the old head and checkpoint remain authoritative and the intermediate revision is
  reused when the same run resumes;
- signal tests showing `SIGTERM`/`SIGINT` reach active work, interrupted mutations are
  not counted as complete, and writer/socket resources are released;
- advisory-lock serialization of socket startup/recovery, device/inode rechecks,
  private lock permissions, live/unexpected-path preservation, concurrent-owner
  exclusion, and non-mutating control-plane inspection; and
- `wikisync doctor` human/JSON output and create-new `0600` bundles using a strict
  aggregate allowlist, immutable read-only checkpointed database access, bounded
  canonical quick verification, redacted section failures, and tests proving no
  MediaWiki connection or seeded sensitive-value disclosure by default; explicit
  `doctor --online` adds a one-request, no-retry, time/size/source-bounded reachability
  probe whose output remains aggregate-only and redacted.

Transport policy, manifests, trust anchors, and export now include:

- an endpoint-derived normalized source-host allowlist and fail-closed redirect policy
  that permits only the configured scheme/host/effective-port origin, including tests
  proving a cross-origin redirect is rejected before the destination is contacted;
- a source-bound, whole-answer DNS policy that disables ambient proxies, rejects
  unsafe literal, empty, mixed, private/special-use, and over-32-address resolutions,
  revalidates every new connection, and reserves plain HTTP plus loopback answers for
  the explicit fixture path;
- clone-shared configurable request concurrency and aggregate response-body ceilings
  (defaults: four in-flight requests and 512 MiB per client/run), enforced across
  retries without weakening the existing per-response, retry, or circuit bounds;
- schema migration 8, which snapshots an immutable configuration hash when each new
  sync run starts, plus bounded canonical JSON manifests with domain-separated BLAKE3
  identities, strict append sequence and predecessor links, catalog-difference
  introduced revisions, resulting page heads, durable atomic installation, and
  idempotent oldest-first repair of the database/file crash gap;
- automatic manifest append at successful bootstrap/reconciliation boundaries and a
  bounded pre-network repair step for previously completed runs, with an integration
  test proving repair and the new append occur without extra source requests;
- full verification of manifest inventory, canonical encoding/body identity,
  sequence/predecessor continuity, duplicate or unsuccessful run references, and
  manifest coverage of eligible successful runs, with structured findings for
  deletion, tampering, swapping, and concurrent inventory changes;
- bounded stable full-verification scans for revision-to-page/object reachability,
  locally present parent ownership/self-reference, page heads, checkpoint collection
  and successful-run scope/boundary, search-document/FTS pointers, orphan FTS rows,
  and the current search transformer version;
- optional Ed25519 PKCS#8 key generation/import plus signing of a validated manifest-
  chain head, bounded canonical public trusted-head export/import, and full
  verification against a separately retained anchor with distinct invalid-signature
  and stale/different-head findings;
- explicit CLI and GUI lifecycle controls for absolute external key/anchor paths,
  protected create-new key generation and validation, fail-before-publish full
  verification, anchor inspection, atomic refresh, and recovery retention. The CLI
  also supports copy-import and phased key rotation that deletes neither old key nor
  recovery anchor;
- CLI `export --format markdown|text [--collection <id>]` for deterministic current
  captured heads, producing bounded private `articles/`, `index.jsonl`, and
  `manifest.json` outputs with transformer versions, content IDs, capture/revision
  provenance, source attribution, staged replacement, and symlink/path protections;
  and
- historical `export --at <revision-or-time>` with distinct deterministic outputs,
  an inclusive per-page captured-revision cutoff, revision-anchor provenance, and an
  indexed `LIMIT 1` store query rather than full-history materialization. Historical
  export never replaces `exports/current`.

Durable network transfer policy now also includes:

- schema migration 9 with one validated library-wide policy for maximum concurrent
  requests, an optional aggregate downloaded-byte rate, and metered-network
  avoidance; updates are atomic, survive restart, and are included in new sync-run
  configuration hashes;
- a clone-shared monotonic byte-rate limiter applied to every streamed response body,
  including retry responses, without weakening the existing per-response or per-run
  byte ceilings;
- GUI controls that read and edit the durable policy through either a direct writer
  lease or a versioned daemon extension, with the daemon and direct GUI sync paths
  applying the same policy to each MediaWiki client; and
- bounded local metered-network detection through NetworkManager on Linux. A known
  metered connection blocks foreground synchronization and leaves overdue schedules
  unclaimed for later; unknown, conflicting, unavailable, and unsupported results are
  fail-visible but do not pretend to prove that a connection is unmetered. macOS
  currently reports unsupported because no reliable safe integration is implemented.

Additional stable-v1 application progress now includes:

- periodic bounded category re-resolution in every daemon collection sync, with a
  network-free no-op for static title rules, atomic membership replacement, retained
  history for removed members, budget enforcement, and failure tests proving an
  incomplete preview never changes membership;
- initial administrative CLI commands for library initialization, validated source
  add/list, safe removal of wholly unused sources, and collection listing, including
  stable JSON inspection output and direct-writer exclusion while protocol version 1
  lacks these mutations; and
- refusal to physically remove a collection under the current schema because its
  foreign key would null historical sync-run scope. Correct collection removal needs
  a tombstone/status migration that preserves manifest and run evidence.

Credential-free beta packaging readiness now includes:

- deterministic, bounded macOS and Linux release-candidate archives containing the
  CLI, daemon, GUI, license, operations documentation, and target service templates;
- strict SHA-256 manifest creation/verification, canonical archive-layout validation,
  and OpenSSH Ed25519 detached-checksum signing/verification hooks with private-key
  type, ownership, permission, and symlink checks;
- five packaging tests covering reproducibility, tampering, unsafe paths, symlinked
  inputs, signing, and verification; and
- a native macOS/Linux CI dry run with an immutable checkout action that builds and
  verifies candidates but deliberately has no credentials, signing, publication, or
  release-write authority.

Workspace formatting, warning-denied Clippy, and all workspace tests pass. The
`cargo-deny` subcommand is unavailable in this environment. All five packaging tests,
release-workflow YAML parsing, and a real local archive/checksum/layout dry run also
pass. This checkpoint adds no new package version; `ring`, `serde`, and `serde_json`
became direct integrity dependencies, `wikisync-content` became a direct workspace
dependency there, and Tokio enabled its existing `net` feature for MediaWiki DNS.
These packages already match the repository's source/license policy. Milestone 4
delivery is still partial: actual Apple signing/notarization, a defined Linux
package/repository trust model, credentialed native release validation, protected
release identities/trust distribution, and signed artifact publication remain.

## Plan audit notes

The ordered first implementation backlog (items 1–13) is complete. The broader
milestone delivery lists are not all closed: the administrative CLI surface is only
partial, credentialed platform packages remain outstanding, and optional media, dump
bootstrap, pack tuning, stable contracts, and release acceptance remain Milestone 5
work. User-facing external signing/trusted-anchor workflows, historical export, and
periodic dynamic-category membership reconciliation are now implemented.
Full verification now covers stable scans of every logical canonical object, the
manifest chain and eligible-run coverage, and the current-schema metadata pointers
listed above. A separately retained signed trusted head can authenticate the observed
chain through the library API. The schema has no `derived_cache` inventory, contentless
FTS permits pointer rather than indexed-body comparison, absent parent revisions can
be valid under retention policy, and missing search documents may be pending derived
work. Verification must not be described as source truth, universal whole-archive
verification, or external rollback protection when the anchor is kept with and can be
replaced alongside the library.

## Next checkpoint

Add a collection tombstone/status migration, preserve historical run/manifest scope,
and complete previewed collection add/edit/remove plus daemon-protocol administration.
Complete credentialed Apple signing/notarization and define the Linux
package/repository trust model before calling the candidate archives signed beta
packages. Optional attributed thumbnail media, current-language dump bootstrap, pack
tuning, stable configuration/database/JSON/export/backup contracts, and release
acceptance on macOS/Ubuntu remain separately tracked Milestone 5 work.

Milestone gates remain tracked in `IMPLEMENTATION_PLAN.md`; an item being complete
means its initial implementation is present, not that its later milestone hardening
is finished.
