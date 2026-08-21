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

## In-progress backlog item

13. Category recursion, long-gap recovery, and the Iced GUI are underway. The first
    implementation slices now include:

    - A bounded, non-mutating category preview with MediaWiki continuation,
      main-namespace filtering, breadth-first subcategory recursion, cycle and
      duplicate handling, deterministic output, and a standalone human/JSON CLI.
    - Durable collection-head reconciliation by stable page ID, including moved-title
      updates, retained missing-page history, bounded forward gap capture from the
      newest durable revision, resumable partial progress, current-search refresh,
      and checkpoint advancement only after every page job succeeds.
    - An initial Iced application with first-run privacy/storage acknowledgement,
      library creation/opening, collection and revision overview, empty collection
      creation through shared store services, synchronization status, and bounded
      recent-object verification. Background results are scoped to their originating
      library, and MediaWiki endpoints are validated before configuration is stored.
    - User-only Unix permissions for library directories and SQLite/object files,
      applied centrally by the store for GUI, CLI, and daemon callers.

    This backlog item is not complete. Category rules and resolved inclusion reasons
    still need schema-backed persistence and article capture; title-list import,
    estimates, budgets, policy editing, sync controls, scheduling, reader launching,
    deletion/restoration log semantics, and full integrity verification are not yet
    exposed through the GUI.

## Next checkpoint

Persist category collection rules and previewed membership with an explicit commit
step, then capture those pages through the existing synchronization services. Add
newline-delimited title-list import in the same collection-service layer before
expanding the GUI editor.

Milestone gates remain tracked in `IMPLEMENTATION_PLAN.md`; an item being complete
means its initial implementation is present, not that its later milestone hardening
is finished.
