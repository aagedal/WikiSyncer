# WikiSyncer implementation status

Updated: 2026-08-29

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
- GUI collection synchronization and collection administration using the same
  forwarding/direct-writer contract, including daemon-owned creation for an already
  configured source;
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

Collection lifecycle administration now also includes:

- schema migration 10 with durable active/tombstoned collection status and a monotonic
  configuration/membership generation. Tombstoning stops tracking, pauses scheduling,
  cancels unfinished scoped work without checkpoint advancement, and retains collection
  identity, configuration, resolved-member history, checkpoints, runs, manifests,
  pages, revisions, and canonical objects;
- all-or-nothing collection create/edit transactions covering name, selection rule,
  history and removal policy, hard budgets, complete resolved membership, unresolved
  titles, and estimates. Accumulated `keep-tracking` membership is included in budget
  enforcement, and stale previews are rejected by generation compare-and-swap instead
  of overwriting later reconciliation or administrative changes;
- daemon protocol version 2 with version-1 compatibility and a bounded, expiring,
  non-durable draft upload. Ordered chunks preserve the 64 KiB frame ceiling while
  exact preflight and incremental allocation bounds cap a complete draft at 16 MiB;
- shared direct/daemon typed operations for estimate, add, edit, and non-destructive
  remove. A newly administered collection takes the bootstrap path before later
  reconciliation, including configured history capture, and `sync all` uses the same
  bootstrap decision;
- CLI `collection add|edit|list|remove|estimate` with complete bounded previews,
  explicit `--commit`, stable versioned JSON, active/all inspection, bounded
  single-handle title-list reads, durable transfer/metered policy, and direct or
  daemon-owned writer execution; and
- an Iced collection editor for complete previewed scope, history, budgets, removal
  policy, and scheduling plus confirmed tombstone removal. Existing-library and
  post-daemon inspection is read-only, and fixture gates prove direct/daemon behavior,
  daemon-owned create-and-bootstrap, and stale-edit rejection.

A cancelled partial run is deliberately not promoted to successful manifest evidence.
Its durable objects remain locally catalogued and hash-verifiable, while the last
successful manifest remains the authenticated boundary.

Daemon-owned source administration now also includes:

- bounded typed protocol-v2 source add/remove operations with protocol-v1 behavior
  unchanged, shared validation for direct and daemon-owned writers, and idempotent
  registration receipts that return the durable source identity and configuration;
- transactional refusal to remove a source referenced by any collection, with tests
  proving the source and its dependent state are unchanged on rejection; and
- CLI forwarding for source add/remove plus GUI registration of a missing source
  before daemon-owned collection creation. Fixture tests cover new-source bootstrap,
  safe unused removal, in-use rejection, and explicit partial receipts when source
  registration succeeds but a later collection budget check fails.

Additional stable-v1 application progress now includes:

- periodic bounded category re-resolution in every daemon collection sync, with a
  network-free no-op for static title rules, atomic membership replacement, retained
  history for removed members, budget enforcement, and failure tests proving an
  incomplete preview never changes membership;
- administrative CLI commands for library initialization, validated source add/list,
  safe removal of wholly unused sources, and the complete planned non-destructive
  collection administration surface. Mutations use the same typed direct/daemon
  boundary while inspection remains read-only; and
- explicit refusal to treat collection removal as destructive purging. Tombstoning
  preserves historical scope and captured data; a separate purge command would still
  require its own bounded preview and evidence-preservation design.

Stable contracts, pack tuning, and dump-import foundations now also include:

- a documented stable-v1 compatibility boundary for general CLI JSON, export
  schemas, forward-only database/configuration migration, and quiescent
  permission-preserving whole-library directory backups. General CLI JSON responses
  now carry `schema_version: 1`, with deterministic golden fixtures; doctor and export
  retain their independent version identifiers;
- schema migration 11 and deterministic pack affinity ordering by object kind,
  wiki/page, logarithmic size class, and revision order; same-page bounded delta
  search, periodic complete anchors, and oversized-candidate isolation. A 100-revision
  fixture packs from 820,100 loose compressed bytes to 222,563 pack-plus-index bytes
  while preserving a deterministic pack identity across ingestion orders; and
- a bounded streaming current-page multistream dump reader using concatenated bzip2
  decoding and pull-based XML parsing without whole-dump disk decompression. It has
  explicit compressed/decompressed/page/text/XML/cardinality limits, namespace and
  content-model filters, strict scalar/structure handling, suppressed-contributor
  protection, and offline fixtures;
- authenticated, restartable acquisition from a caller-retained BLAKE3 index anchor.
  The strict bounded index transitively commits the ordered dump artifacts, database,
  generated timestamp, lengths, and BLAKE3 identities. Downloads reuse the existing
  source-bound DNS, proxy, redirect, concurrency, byte-rate, aggregate-byte, and
  timeout policy; validate exact range resumption; publish private regular files
  durably without overwrite; and re-hash the opened handle before import; and
- schema migration 13 plus a current-and-future dump-bootstrap application path with
  exact dump-set/configuration/generation binding, monotonic multipart resume cursors,
  idempotent selected-page ledgers, aggregate parser limits, hard collection budgets,
  stable-page-ID filtering, source/client/site validation, and atomic permanent-failure
  terminalization. Every selected ID is queried after the scan; absent pages use the
  Action API and changed imported heads reuse forward-gap reconciliation so all public
  intermediate revisions are durable before the bootstrap checkpoint and manifest
  complete. Ordinary bootstrap jobs cannot be adopted as dump work.

Authenticated current-dump bootstrap now also has an end-to-end product workflow:

- a typed, bounded shared service previews the configured source, independently
  retained BLAKE3 index identity, resolved stable-page-ID scope, durable network
  policy, acquisition/parser/storage ceilings, private cache location, collection
  generation, and hard page/canonical-byte budgets without contacting the source;
- execution is bound to the previewed collection generation, preserves the
  `TrustedDumpIndex` to authenticated acquisition to `VerifiedDumpSet` boundary, and
  uses the same direct short-writer or daemon-owned writer discovery as other
  mutations. A versioned bounded daemon extension preserves protocol-1/2 behavior;
- `wikisync dump-bootstrap` is preview-only by default and requires explicit
  `--commit`, a collection ID, index URL, caller-retained BLAKE3 index digest, and
  expected database. Human and stable JSON output explicitly warn that a checksum
  downloaded or stored beside the index is not an independent trust anchor;
- Iced provides separate preview and confirmed-start controls for the same trust,
  scope, limits, and budgets, including safe async direct execution without nesting a
  runtime; and CLI/GUI status surfaces durable dump identity, cursor, imported pages
  and bytes, attempts, retryability, and structured failure state; and
- offline fixtures cover direct execution, daemon forwarding, GUI execution inside
  its Tokio task, and a retryable closure failure followed by a real daemon restart.
  The restart re-authenticates the index, reuses the verified cached artifact, and
  resumes the same run/import identity without counting partial work as complete.

Optional-thumbnail capture now forms an end-to-end, default-off stable-v1 path:

- schema migration 12 adds default-off, generation-tracked bounded thumbnail policy,
  rendition-aware immutable media metadata, revision placements, attribution,
  licensing, capture provenance, and canonical media-object references. Collection
  create/edit persists the policy atomically with scope, membership, history, budgets,
  and generation through both direct and daemon-owned paths;
- Iced exposes default-off or bounded thumbnail limits, while daemon draft version 2
  carries the policy within the existing frame and total-draft bounds and preserves
  legacy draft/protocol behavior;
- JPEG and passive PNG bytes must pass MIME/magic agreement, structural completion,
  animation rejection, bounded full decoding, pixel/dimension/allocation ceilings,
  and exact decoded-metadata agreement before storage. Multiple renditions are
  retained, and a same-byte retry preserves the first capture time;
- full verification now performs bounded stable scans of media and placement metadata,
  verified object reads, complete raster validation, rendition/reference ownership,
  and stable-v1 metadata bounds; and
- MediaWiki support can discover a bounded exact-revision passive-image list, resolve
  typed thumbnail/attribution/license/hash metadata, and download same-origin bytes
  through the existing semaphore, DNS, redirect, retry, circuit, rate, per-image, and
  per-run limits;
- recognized standard-port HTTPS Wikimedia project sources derive exactly one
  additional `https://upload.wikimedia.org:443` origin. Third-party, loopback, and
  nonstandard sources remain exact-origin; approved CDN connections retain whole-
  answer public DNS validation, new-connection revalidation, proxy exclusion, and
  exact-origin redirect enforcement;
- bootstrap, historical-policy capture, and long-gap reconciliation discover media
  only after canonical text is durable, reuse existing placements, download and fully
  validate missing renditions, enforce distinct-object collection byte budgets, and
  attach immutable attribution/licensing metadata. Optional per-image failures are
  reported by stage without invalidating captured text, while local catalog/I/O
  failures still stop the operation;
- manifest schema 2 deterministically authenticates media inventory and revision
  placements, including metadata, rendition identity, captions/alternative text, and
  capture provenance. Schema-1 manifests remain readable with explicit no-media
  coverage, and full verification reports media deletion, addition, tampering,
  placement swaps, and concurrent inventory changes; and
- current and historical reader pages serve only hash-verified, fully revalidated
  local JPEG/PNG bytes through bounded routes and display source, author/credit,
  license, dimensions, capture time, upstream hash, and local identity. Current,
  historical, and collection exports use explicit v2 schemas with deterministic
  attributed media sections and deduplicated hash-addressed private media files.

The destructive-purge product path now also includes:

- a documented product and threat-model contract that defines purge as
  collection-exclusive canonical-payload reclamation for a tombstoned collection
  while retaining audit metadata and hashes. It requires exact preview confirmation,
  explains shared-reference and pack-delta constraints, and makes no personal-data,
  backup, snapshot, SSD-remnant, or secure-erasure promise;
- schema migration 14 with a durable, bounded purge authorization and future cleanup
  journal. A deterministic read-only preview closes over retained collection
  membership, cross-page/object deduplication, media references, other manifest
  scopes, active verified locations, pack partitions, and the current manifest head,
  then commits the exact
  name, generation, tombstone, inventory, estimates, and pack work into a
  domain-separated BLAKE3 fingerprint;
- tombstone-only authorization that requires the exact name and fingerprint plus
  separate payload-only/audit-retention and external-copy acknowledgements. It
  rederives the complete closure inside an immediate transaction, rejects stale
  catalog/head/location state without partial journal writes, is idempotent for an
  exact retry, and exposes bounded journal pagination; and
- full verification replay of every readable synchronization manifest's introduced
  revision claims and positive historical page-head claims against retained page,
  revision, ownership, and exact content-object metadata. Missing or changed claims
  now have distinct structured findings, while a legitimately superseded historical
  head remains valid;
- manifest schema 3 with backward reads of schema-1/2 synchronization entries and a
  typed purge event bound to the exact durable journal, pre-purge manifest head,
  independently computed bounded logical-catalog fingerprint, and ordered object/pack
  inventory. Cross-process append serialization, no-clobber installation, directory
  durability, duplicate-event rejection, and tamper/stale-head tests protect the
  shared manifest chain;
- schema migration 15 with exact positive authorized-absence evidence, restartable
  file and accounting journals, strict phase invariants, and fail-closed migration of
  legacy schema-14 authorizations that lacked an independent catalog commitment;
- a library-level cleanup executor for verified loose objects and whole-target packs.
  It commits logical absence before physical retirement, resumes `unlinking` work,
  checks expected versus observed bytes and pack replacement metrics, pins managed
  parent and leaf descriptors with no-follow traversal, and uses descriptor-relative
  unlink plus parent-directory durability so ancestor replacement cannot redirect a
  deletion; and
- bounded retained-subset replacement for mixed packs. One cleanup checkpoint fully
  reconstructs and hash-verifies the old pack, builds an immutable replacement with
  exactly the retained object closure, verifies its payload and index, and atomically
  activates checked replacement metrics before the old generation becomes eligible
  for the existing restartable retirement phases;
- full verification that distinguishes exact active authenticated absence from
  unexplained loss, pending or inconsistent cleanup, shared-reference violations, and
  a later verified reintroduction of identical canonical bytes. Reintroduction
  atomically restores the deterministic loose location and supersedes, rather than
  deletes, the historical absence evidence.

The separate preview-first purge operation is now exposed through the CLI, daemon,
and Iced GUI without changing non-destructive `collection remove`. All surfaces use
the same bounded mutation contract, require the exact previewed name and fingerprint
plus separate payload/audit-retention and external-copy acknowledgements, retain one
writer boundary through terminal cleanup, and report checked retirement/replacement
metrics. Daemon startup and every conflicting mutation resume unfinished purge work
or fail closed before proceeding. GUI confirmations begin unchecked and reset when a
preview changes; preview and execution remain local and contact no MediaWiki source.
The library retains audit rows, object identities, hashes, and manifest evidence and
makes no backup, snapshot, personal-metadata, SSD-remnant, or secure-erasure promise.
Portable POSIX still leaves a very small final leaf-name race between identity
confirmation and `unlinkat`, although pinned parent traversal prevents ancestor
redirection.

Milestone 2 acceptance evidence now also includes a fixture-backed offline lifecycle
that preserves a stable page ID and earlier canonical revision/object identities
through a title move, missing/deleted observation, and restoration, while capturing
both the restored intermediate revision and final head before checkpoint advancement.

Credential-free beta packaging readiness now includes:

- deterministic, bounded macOS and Linux release-candidate archives containing the
  CLI, daemon, GUI, license, operations documentation, and target service templates;
- strict SHA-256 manifest creation/verification, canonical archive-layout validation,
  and OpenSSH Ed25519 detached-checksum signing/verification hooks with private-key
  type, ownership, permission, and symlink checks. Whole-release verification
  authenticates the checksum manifest first, rejects unsigned extra archives, and
  binds checksum plus layout validation to one immutable archive snapshot;
- a defined Linux upstream-archive/public-key trust model with explicit independent
  anchor, rotation, APT/RPM repository-key, and third-party package boundaries, while
  making clear that no production identity or native repository currently exists;
- a credential-free macOS gate with deterministic Mach-O signing plans, structural
  thin/fat executable validation, exact identifiers/architectures/identity inputs,
  fail-closed Developer ID inspection, strict accepted-notarization receipt checks,
  and signed-input-to-final-archive binding. Routine CI uses only an unmistakable
  unprovisioned identity and all-zero fingerprint; and
- twenty-seven packaging and service-policy tests covering reproducibility, tampering,
  unsafe paths, symlinked inputs, Ed25519/signature constraints, exact signed sets,
  path-substitution races, Mach-O and ELF structure, signing policy, notarization
  receipts, archive binding, native systemd parsing, and network-interposer
  enforcement; and
- a native macOS/Linux CI dry run with an immutable checkout action that builds and
  verifies candidates but deliberately has no credentials, signing, publication, or
  release-write authority. Its native Ubuntu path now validates exact 64-bit ELF
  architecture and rendered user units with `systemd-analyze`, while the successful
  job summary binds the credential-free result to the commit, clean tree, host,
  toolchain, lockfile, timestamps, offline audit, and archive checksum;
- a native release-mode offline audit that denies and records IPv4/IPv6 connection,
  addressed datagram, and hostname-resolution attempts while exercising offline CLI
  commands, idle daemon IPC, a browser-like crawl of the default reader, and a bounded
  no-action launch of the packaged Iced GUI. The integrated macOS Aqua run covered six
  reader routes plus three seconds of GUI initialization with zero outbound attempts;
  headless Linux explicitly reports that GUI evidence is unavailable; and
- immutable commit-SHA pins for every third-party action in normal and release CI.

Beta robustness and migration evidence now also includes:

- maintained bounded fuzz targets and seed corpora for deterministic wikitext/plain-
  text/Markdown rewriting, canonical trusted-head JSON, current-dump parsing, loose-
  object decompression, pack/index reads, repacking, delta reconstruction, and every
  currently consumed MediaWiki Action API success/error response shape and
  continuation. The Action API target is feature-gated out of normal application
  builds but shares the production operation validators, including exact caller
  identity checks. Typed deserialization caps one response at 50 pages, 500 revisions,
  500 category members, 4,096 image references, and the single-result image-info
  contract; and
- a retained materialized schema-11 whole-library fixture with two canonical loose
  objects. Public read APIs snapshot its source, collection, membership, schedule,
  transfer policy, titles, revisions, and exact bytes before migrating a copy through
  schema 15 and proving the same state after migration and an idempotent reopen.

Additional release-acceptance hardening now also includes:

- a representative fixture-backed multi-language GUI lifecycle that creates and
  synchronizes an English collection through the direct writer and a Norwegian
  collection through the daemon writer. Deliberately colliding upstream page and
  revision IDs remain source-scoped, both canonical payloads survive restart, and a
  full integrity pass verifies the resulting library;
- kernel-derived Unix-socket peer credential checks before daemon request reads or
  dispatch. macOS `getpeereid` and Linux `SO_PEERCRED` effective UIDs must match the
  daemon owner and lookup failures fail closed. This protects the cross-account
  boundary; hostile processes already running as the same account remain explicitly
  inside the trust boundary;
- maintained named seed corpora for all six fuzz targets, including ordinary empty,
  short, Unicode, nested, incomplete, and valid samples. Every target compiled and
  completed a bounded direct smoke run. The original five completed clean 60-second
  nightly AddressSanitizer/libFuzzer resource-sizing campaigns, and the direct
  untrusted Action API JSON boundary completed 875,058 clean executions in a separate
  61-second campaign. Exact corpus identities, coverage, peak RSS, toolchains, and the
  symbolizer limitation are recorded in `docs/benchmarks/fuzz-campaign-2026-08-25.md`
  and `docs/benchmarks/fuzz-campaign-2026-08-29-action-api-json.md`. Longer sustained
  native campaigns remain outstanding;
- the Action API fuzz boundary now accepts the production response ceiling rather
  than silently stopping at the routine 256 KiB campaign size. A deterministic,
  symlink-safe generator derives all seven response kinds at exactly 8 MiB plus the
  harness selector and adds the typed 50-page, 500-revision, 500-category-member, and
  4,096-image-reference maxima without committing large fixtures. Its five tests pass,
  and an 11-input full-ceiling bounded smoke completed cleanly at 422 MiB peak RSS;
  this does not substitute for a sustained campaign;
- the missing written renderer evaluation and fixture-backed Milestone 0 API/disk
  characterization artifacts, which record the conservative offline renderer choice,
  its fidelity boundary, reproducible commands, and non-SLA measurements; and
- a per-candidate macOS/Ubuntu release-acceptance matrix that separates actual native
  results from workflow definitions and credential-free evidence from production
  signing, notarization, clean-system, trust-distribution, and publication gates. The
  current local macOS row records the integrated workspace, packaging, migration,
  multi-language, fuzz-build/smoke, release offline-audit, and rootless archive-
  rehearsal results. The pinned native release workflow binds each candidate archive
  and SHA-256 to its generated evidence, retains the complete credential-free evidence
  set for 30 days, and keeps read-only repository permissions. Ubuntu remains pending
  actual native CI evidence for this working tree;
- a bounded rootless native install/upgrade rehearsal that snapshots and validates
  current or explicitly legacy-layout archives, extracts them into isolated private
  slots, checks the stable administration surface, initializes and inspects a fresh
  private library, exercises daemon health/status/shutdown, renders private user-
  service assets, and emits create-new deterministic JSON evidence. The current macOS
  archive path passes. The optional older-candidate path migrates before read-only
  comparison and is fixture-tested, but no eligible older packaged candidate with the
  stable administration CLI was available for a real archive-to-archive run. The
  rehearsal neither installs/enables a service nor claims clean-system certification;
- one bounded application User-Agent policy for every production MediaWiki and dump
  client. `WIKISYNC_OPERATOR_CONTACT` accepts a non-secret public contact, rejects
  malformed or oversized values before contact without echoing them, and safely
  defaults to the project repository URL; service-manager configuration is documented;
  and
- bounded service-log retention evidence: macOS candidate archives contain an hourly
  user-owned launchd companion that cooperatively unloads the daemon before `newsyslog`
  rotation and retains four compressed generations per stream. Linux units use
  journald plus per-unit rate limits while documentation accurately leaves global byte
  and age quotas to administrator policy. Neither platform claims a hard per-service
  disk quota; and
- the offline vertical-slice gate now rebuilds a fresh Markdown export after the
  fixture source is shut down and unreachable, then verifies revision and transformer
  identities, rebuilt article content, source/attribution, and content-hash metadata.

Workspace formatting, warning-denied Clippy, and all workspace tests pass.
`cargo-deny 0.20.2` reports advisories, bans, licenses, and sources clean; expected
duplicate-version and unused-license-allowance diagnostics remain warnings. The
packaging suite has thirty-three tests: thirty-two pass on this macOS host and the
native-Linux systemd test is skipped as designed. Workflow YAML parsing,
immutable-action-pin checks, fuzz-target compilation, smoke and bounded instrumented
runs, fixture checksums, the native release offline audit, and final packaging
verification pass.
This checkpoint adds no new package version. Purge cleanup now directly uses `rustix`
for safe descriptor-relative filesystem operations. Dump parsing/acquisition directly
uses `bzip2`, `quick-xml`, `blake3`, `bytes`, `fs2`, and `libc`; sync test coverage
adds fixture-only `blake3` and `bzip2`, source comparison uses `url`, and daemon peer-
credential lookup uses safe `nix` wrappers over the supported platform APIs. Bounded
raster validation continues to use `image` with only JPEG/PNG features. Their locked
sources and licenses match the repository policy, and the automated `cargo-deny`
audit passes. Milestone 4 delivery is still partial: actual Apple
signing/notarization, production Linux/macOS identities and independent trust
distribution, credentialed native validation, clean-system Gatekeeper assessment,
and signed artifact publication remain.

## Plan audit notes

The ordered first implementation backlog (items 1–13) is complete. The broader
milestone delivery lists are not all closed: destructive purge now has its validated
mixed-pack and CLI/daemon/GUI completion path, while credentialed platform packages,
additional beta robustness evidence, and final platform acceptance remain outstanding. Pack
tuning, the initial
stable-contract definition, optional thumbnail media, and the authenticated durable
dump-bootstrap library and daemon/CLI/GUI product paths are implemented.
Daemon-owned source administration, the planned
non-destructive collection CLI/GUI surface, user-facing external
signing/trusted-anchor workflows, historical export, and periodic dynamic-category
membership reconciliation are implemented.
Full verification now covers stable scans of every logical canonical object, the
manifest chain and eligible-run coverage, retained introduced-revision and positive
historical page-head claims, and the current-schema metadata pointers listed above,
including media objects and placements. A separately retained signed
trusted head can authenticate the observed revision/manifest chain and schema-2 media
snapshot through the library API; schema-1 manifests explicitly provide no media
coverage. The schema has no `derived_cache` inventory,
contentless FTS permits pointer rather than indexed-body comparison, absent parent
revisions can be valid under retention policy, and missing search documents may be
pending derived work. Verification must not be described as source truth, universal
whole-archive verification, authenticated media history, or external rollback
protection when the anchor is kept with and can be replaced alongside the library.

## Next checkpoint

The initial robustness checkpoint and its next local evidence slice are complete:
maintained fuzz targets and seed corpora, a native release-mode offline/outbound audit,
a real older-beta whole-library migration fixture, a representative multi-language
GUI/daemon lifecycle, a recorded macOS/Ubuntu acceptance matrix, and the historical
Milestone 0 API/disk and renderer-evaluation artifacts are present. Initial
instrumented fuzz resource outcomes and a production-ceiling Action API seed/smoke
mechanism are now recorded. The next locally actionable release-acceptance work is
longer sustained campaigns using those production-sized inputs, native Ubuntu
execution for the current candidate matrix, and a clean-system macOS/Ubuntu install,
service-manager, and upgrade assessment once signed candidate artifacts are available.

The credential-free security-exit slice named by the previous checkpoint is now
implemented: bounded operator-contact configuration reaches every production source
client, platform service-log policy and limitations are documented and tested, and the
native macOS audit includes the packaged GUI default launch. Daemon peer credentials
enforce the cross-account boundary and the documented hostile-same-UID limitation
remains by design. The native audit still does not cover GUI actions after launch or
explicitly requested online operations, and native Ubuntu graphical evidence remains
pending.

Credentialed Apple signing/notarization, protected production release identities and
independent trust distribution, native release validation, clean-system assessment,
and signed publication must still succeed before candidate archives can be called
signed beta packages; those external actions remain unauthorized/unavailable in this
checkpoint. Release acceptance on macOS/Ubuntu also remains separately tracked
Milestone 5 work.

Milestone gates remain tracked in `IMPLEMENTATION_PLAN.md`; an item being complete
means its initial implementation is present, not that its later milestone hardening
is finished.
