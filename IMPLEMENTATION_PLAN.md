# WikiSyncer Implementation Plan

Status: Final planning baseline
Primary targets: macOS and Linux
Implementation language: Rust
User interfaces: Iced GUI, CLI, and local read-only website

## 1. Product definition

WikiSyncer creates a selective, tamper-evident offline history of Wikipedia source
revisions. It presents the captured material through a purpose-built reader optimized
for offline reading, full-text search, revision comparison, and use by local AI tools.

The local reader is not intended to reproduce Wikipedia's visual design or exact
server-side rendering. Raw wikitext is the canonical historical record. Markdown,
plain text, HTML, search indexes, and AI exports are derived representations that can
be rebuilt locally.

### 1.1 Required capabilities

- Read and search all synchronized material without a network connection.
- Select individual pages, title lists, categories, or curated collections.
- Download only newly selected pages and new revisions of changed pages during normal
  synchronization. A newly discovered revision may require downloading its complete
  wikitext because Wikimedia does not expose a durable binary-delta feed.
- Retain every captured intermediate revision and its available public metadata.
- Compare arbitrary revisions using line- and word-level diffs.
- Detect local modification or corruption after capture.
- Export clean Markdown and plain text for local AI systems.
- Configure collections, history depth, schedules, and storage budgets through Iced.
- Support unattended operation through a CLI and background daemon.
- Serve a local, read-only encyclopedia website.

### 1.2 Explicit non-goals for the first stable release

- Pixel-identical Wikipedia rendering.
- Reproducing historical template, Lua, Wikidata, or MediaWiki parser behavior.
- Downloading all English Wikipedia revision history as a normal consumer workflow.
- Treating Wikipedia content or a cryptographic hash as proof of objective truth.
- Editing Wikipedia or the local archive through the reader.
- Synchronizing talk, user, or file namespaces by default.
- Windows packaging. The architecture should remain portable, but macOS and Linux are
  the release targets.

## 2. Fidelity and trust model

WikiSyncer distinguishes three forms of data:

1. **Canonical evidence**: exact public wikitext and revision metadata returned by the
   configured MediaWiki source.
2. **Derived content**: normalized text, Markdown, locally rendered HTML, link graphs,
   captions, and search indexes.
3. **Local provenance**: capture time, source endpoint, request identifiers, content
   hashes, sync manifests, and software version.

Integrity verification proves that captured bytes have not changed since a trusted
manifest was recorded. It does not prove that a statement is accurate, unbiased, or
free from upstream manipulation. The UI must consistently say "captured," "observed,"
or "verified since capture" rather than claiming to store absolute truth.

A revision that later becomes unavailable upstream remains in the local archive, but
its upstream status is shown clearly. Setup documentation must warn that this may
retain material later suppressed for privacy, copyright, or safety reasons.

## 3. Delivery scope

### 3.1 MVP collection limits

The first usable release should be designed and tested for collections up to 10,000
main-namespace pages in one Wikipedia language. This is a validation target, not an
enforced limit. The storage and search interfaces must permit later scaling.

MVP selection modes:

- Explicit page titles.
- Newline-delimited title import.
- One category with configurable recursion depth and a preview before commitment.

MVP history policies:

- Current revision plus all future revisions.
- Last N revisions.
- All revisions since a selected date.
- Complete available public history for explicitly selected pages.

### 3.2 Stable v1 scope

- Multiple collections and Wikipedia languages.
- Storage/page-count estimates and hard collection budgets.
- Periodic category reconciliation.
- Optional thumbnail images with captions and license metadata.
- macOS and Linux background scheduling and packages.
- Current-language dump import for large collections.

## 4. System architecture

```text
Wikimedia Action API / REST API / dumps
                    |
          MediaWiki source adapter
                    |
       selection resolver + sync engine
          |             |             |
       metadata     object store    derived views
        SQLite     wikitext/media  search/cache/export
          |             |             |
          +-------------+-------------+
                        |
                   search index
                        |
              +---------+---------+
              |         |         |
             CLI    Iced GUI   local website
                        |
                 background daemon
```

The daemon is the only long-lived writer. GUI and CLI commands either perform a
short, exclusively locked operation when the daemon is absent or forward mutating
operations to the daemon. Readers may operate concurrently using SQLite WAL mode and
immutable content-object files.

The storage model deliberately follows Git's separation between logical history and
physical representation. A revision points to an immutable content object ID; it does
not know whether that object is currently stored loose, as a complete object in a
pack, or as a delta in a pack. Repacking therefore cannot change revision identity,
history, or integrity manifests.

## 5. Repository structure

Begin with a Cargo workspace:

```text
apps/
  wikisync-cli/          Automation and administration commands
  wikisync-gui/          Iced desktop application
  wikisyncd/             Scheduler and single-writer service

crates/
  wikisync-core/         Domain types, policies, configuration, errors
  wikisync-mediawiki/    APIs, pagination, throttling, dump readers
  wikisync-store/        SQLite, content objects, pack indexes and compaction
  wikisync-sync/         Planning, checkpoints, reconciliation, jobs
  wikisync-content/      Wikitext normalization, Markdown, captions
  wikisync-search/       Search abstraction and SQLite FTS implementation
  wikisync-integrity/    Hashing, manifests, signatures, verification
  wikisync-web/          Axum routes, templates, and offline assets

fixtures/
  mediawiki/             Recorded and hand-authored API fixtures
docs/
  architecture/          Architecture decision records and threat model
```

Avoid creating a crate for every small abstraction. Consolidate crates if the initial
implementation shows that boundaries do not provide independent testing or ownership
value.

## 6. Storage design

### 6.1 Filesystem layout

Use platform-standard application data directories. A library has this logical shape:

```text
library/
  library.sqlite3
  objects/
    loose/b3/ab/cd/<blake3>
    packs/
      pack-<id>.pack
      pack-<id>.idx
  manifests/
    000000000001.json
  cache/
  exports/
  tmp/
```

New canonical content is first written as a loose object using a temporary file,
bounded streaming compression, `fsync`, hash verification, and atomic rename before
the referencing SQLite transaction commits. Incomplete temporary files are cleaned or
resumed safely after startup. Packs are built beside the active object set, fully
verified, and activated atomically before superseded loose objects or packs become
eligible for garbage collection.

### 6.2 Core tables

- `schema_migrations`
- `wikis`: source identity, base URLs, language, namespaces, site metadata
- `collections`: name, selection rule, history policy, image policy, storage budget
- `collection_pages`: resolved membership and inclusion reason
- `pages`: wiki/page ID, namespace, current title, current revision, state
- `page_titles`: title and move observations over time
- `revisions`: revision ID, page ID, parent ID, timestamp, author, comment, flags,
  upstream SHA-1, content model, canonical `content_object_id`, local capture metadata
- `content_objects`: BLAKE3 identity, uncompressed length, media type and verification
  state; physical encoding and location are deliberately not part of revision identity
- `object_locations`: loose or pack location, encoding, base object where applicable,
  compressed length and pack generation
- `packs`: immutable pack identity, index checksum, creation state and verification
  state
- `derived_cache`: optional bounded entries keyed by content object, transformer
  version and output kind; safe to delete without losing canonical data
- `media`: file identity, URL, dimensions, MIME type, caption, attribution, license,
  content object ID
- `page_media`: revision-to-media relationship and placement metadata
- `sync_checkpoints`: recent-change cursor, overlap timestamp, reconciliation state
- `sync_runs`, `sync_jobs`, and `sync_errors`
- `integrity_manifests`
- FTS5 tables for the selected current revision of each page

All remote identities use `(wiki_id, remote_id)` composite uniqueness. Titles are not
identities because pages can move.

### 6.3 Git-inspired revision object model

Revision history and content storage are separate logical layers:

```text
Revision 100 -> content object A
Revision 101 -> content object B
Revision 102 -> content object C
Revision 103 -> content object A  # exact revert; no duplicate content
```

The versioned content object ID uses a domain-separated envelope such as
`b3:BLAKE3("wikisync-object-v1\0" || kind || length || canonical bytes)`. Revision
records form the authoritative parent-linked history and point only to that ID.
Identical content of the same kind across reverts, pages, imports, or collections is
stored once. The application can change an object's physical encoding without
changing its ID or any revision record.

Ingestion favors correctness:

1. Receive complete revision wikitext from Wikimedia when its content object is not
   already present.
2. Verify upstream metadata and calculate the local object ID.
3. Store one independently Zstandard-compressed loose object atomically.
4. Commit the revision record pointing to the object ID.
5. Make the revision immediately readable; packing is never on the critical capture
   path.

A background packer provides Git-like physical compaction for stable v1:

- Group candidate objects primarily by wiki/page and similar size, while allowing an
  object to use any verified similar object as its physical base.
- Choose between a complete compressed representation and a binary delta according to
  measured size; logical revision parents do not constrain physical delta bases.
- Keep complete objects at bounded intervals and bound delta depth and reconstruction
  work.
- Require every delta base to precede its dependent entry in the pack/object graph, so
  physical dependencies are acyclic and independently verifiable.
- Write immutable packs and a separate object-ID-to-offset index.
- Reconstruct and verify every packed object's BLAKE3 identity before activating a
  pack.
- Keep the old representation valid until the new pack and index are durable and
  atomically visible.
- Make interrupted compaction restartable and make repacking transparent to readers.

This is inspired by Git's object and pack separation, but WikiSyncer does not use a Git
repository. Git commits and trees represent whole repository snapshots and do not map
cleanly to MediaWiki page IDs, independent page revision streams, categories, search,
or synchronization state.

### 6.4 Sync manifests and library snapshots

Every successful synchronization appends a predecessor-linked manifest containing the
source, capture interval, configuration hash, introduced revisions, and resulting page
heads. For small libraries it may contain the page-head mapping directly. At larger
scale, store the mapping as a content-addressed Merkle tree so unchanged subtrees are
reused between manifests.

Manifests refer to logical revision and content IDs, never pack offsets. Repacking
therefore leaves the recorded library state and signatures unchanged.

### 6.5 AI-facing files

Maintain or generate this export on request:

```text
exports/current/
  articles/<page-id>-<slug>.md
  index.jsonl
  manifest.json
```

Markdown front matter contains the wiki, page ID, title, revision ID, revision time,
source URL, capture time, and content hash. Current exports may be maintained
incrementally. Historical exports are generated for a requested page, collection, or
time slice rather than producing millions of ordinary files automatically.

## 7. Content processing

### 7.1 Canonical input

Never rewrite canonical wikitext. Validate UTF-8, declared content model, response
limits, remote SHA-1 where possible, and local BLAKE3 before committing it.

### 7.2 Derived article representation

Create a deterministic local pipeline from canonical wikitext to a transient parsed
document:

1. Parse structural wikitext constructs.
2. Preserve headings, paragraphs, lists, quotations, code, tables, references, and
   internal/external link targets.
3. Extract readable values from a conservative allowlist of common templates.
4. Render unknown or complex templates as a compact labeled placeholder, optionally
   retaining safe textual arguments.
5. Render the parsed document directly to normalized text, Markdown, or sanitized HTML
   using only bundled CSS.

Markdown is an interchange/export format, not a second internal source of truth. The
search index is persisted, but normalized source text need not be stored separately.
Reader HTML and parsed documents are generated on demand and may use a bounded,
disposable cache keyed by `(content_object_id, transformer_version, output_kind)`.
Historical revisions are normally uncached. A transformer update invalidates affected
cache entries without contacting Wikimedia.

### 7.3 Diffs

Provide two modes:

- **Exact source diff** over canonical wikitext.
- **Reading diff** over normalized Markdown/plain text.

Use line-level diff with word-level highlighting inside changed lines. Compute diffs
locally so arbitrary archived revisions remain comparable offline. Diffs are cached
only as an optimization.

### 7.4 Images

The default image policy is `none`. Later policies are:

- `thumbnails`: bounded-size lead and inline images.
- `full`: higher-resolution media within an explicit byte budget.

When an image is captured, store caption, alt text, source description URL, author,
license, dimensions, MIME type, capture time, and content hash. The article remains
usable when media is absent by displaying a captioned placeholder. SVG and other
active formats must be rasterized or served under a restrictive policy rather than
embedded unsafely.

## 8. Synchronization protocol

### 8.1 Initial bootstrap

For small and medium selections:

1. Resolve titles and categories to stable page IDs.
2. Capture site metadata and the start-of-bootstrap timestamp.
3. Enumerate revisions according to each page's history policy.
4. Fetch revision metadata first and reuse an existing content object when its verified
   upstream hash identifies content already captured locally.
5. Fetch otherwise-missing revision content with bounded pagination and concurrency,
   commit it as a loose object, and then commit the revision reference.
6. Query changes since the bootstrap start time to close the race window.
7. Build derived content and search entries.
8. Commit the final checkpoint and integrity manifest.

For large current-only selections, add a streaming multistream-dump importer in v1.
Do not decompress an entire dump to disk. Full-history dump import remains an advanced
workflow with explicit storage warnings.

### 8.2 Normal update

1. Start from a timestamp earlier than the last committed checkpoint to create an
   overlap window.
2. Enumerate recent changes forward and deduplicate by remote change/revision ID.
3. Filter by selected page IDs and relevant category/log events.
4. Fetch all missing revision metadata for selected changed pages. Reuse an existing
   verified content object for an exact revert; otherwise fetch the full wikitext.
5. Validate that parent links reconnect to locally known history; backfill any gap.
6. Record moves, redirects, deletions, restorations, and category changes.
7. Atomically store new loose content objects and revision references.
8. Regenerate current derived content and FTS entries.
9. Download permitted images after text commits, so media failure cannot block text.
10. Commit the checkpoint and append a sync manifest.

Packing is scheduled after successful capture or during idle maintenance. A pack
failure cannot roll back or invalidate a completed synchronization.

Network concurrency is bounded and configurable. Unattended requests use a descriptive
User-Agent, `maxlag`, server-requested delays, exponential backoff with jitter, and a
circuit breaker for persistent errors.

### 8.3 Recovery after a long offline interval

RecentChanges cannot be the sole recovery mechanism because Wikimedia retains it for
a limited period. When the cursor is too old or periodic verification is due:

1. Batch-query the current title and head revision for every selected page ID.
2. For each differing head, enumerate forward from the newest local revision and fetch
   every missing intermediate revision.
3. Reconcile page moves, missing/deleted pages, and restorations.
4. Re-resolve dynamic category collections and apply their configured removal policy.
5. Verify revision chains and current indexes.
6. Record the reconciliation in the integrity manifest.

Collection removal defaults to "stop tracking but retain captured history." Destructive
purging is a separate, explicit command with a preview.

### 8.4 Checkpoint and failure rules

- A checkpoint never advances beyond durable source content.
- Revision records refer to logical content object IDs, never physical pack locations.
- Every job is idempotent and keyed by remote identity plus operation.
- Retrying the same API page or revision cannot duplicate data.
- Loose-object ingestion and pack activation are independently crash-safe.
- Derived-content and search failures are retryable local jobs; they do not invalidate
  successfully captured canonical content.
- Cancellation completes or rolls back the current bounded transaction and leaves a
  resumable job state.

## 9. Search

Use bundled SQLite FTS5 initially with title, aliases, headings, body, category names,
and captions as separately weighted fields. Search only the selected current revision
by default, with filters for wiki, collection, namespace, and capture/revision date.
Use a contentless/external-content index so FTS persists tokens and positions rather
than a second complete normalized article body. Generate result snippets from the
canonical object or a bounded transient cache.

Expose search through a Rust trait. Before replacing FTS5, benchmark realistic 10,000,
100,000, and 1,000,000-page datasets. Tantivy is the planned alternative if indexing
time, database size, or query latency becomes unacceptable.

## 10. Interfaces

### 10.1 CLI

Planned command surface:

```text
wikisync init
wikisync source add|list|remove
wikisync collection add|edit|list|remove|estimate
wikisync sync [--collection <name>]
wikisync status [--json]
wikisync search <query> [--json]
wikisync show <title> [--revision <id>]
wikisync history <title>
wikisync diff <from-revision> <to-revision>
wikisync export --format markdown|text --at <revision-or-time>
wikisync serve
wikisync verify [--full]
wikisync doctor
wikisync daemon
```

Inspection commands support stable JSON output. Mutating commands provide dry-run or
preview output where the scope can be large.

### 10.2 Iced GUI

Screens and flows:

- First-run privacy, data-directory, and disk-space setup.
- Wikipedia language/source selection.
- Collection builder with page count and size estimate.
- History-depth, category-removal, image, bandwidth, and storage policies.
- Schedule editor and background-service status.
- Sync progress with resumable per-page errors.
- Collection dashboard and recent captured changes.
- Integrity status and verification actions.
- Button to open the local reader in the system browser.

The GUI consumes the same application service interfaces as the CLI and must not
contain a second implementation of sync or storage logic.

### 10.3 Local website

Use Axum and server-rendered templates with bundled static assets. Initial routes:

```text
/
/search
/wiki/:title
/page/:page_id/history
/revision/:revision_id
/diff/:from/:to
/changes
/collections
/about/source-and-integrity
```

The site binds to `127.0.0.1` by default, exposes no mutation routes, loads no remote
fonts/scripts/styles, rewrites internal article links locally, and labels unsynchronized
targets. HTML is sanitized and served with a restrictive Content Security Policy.
Serving on a LAN is an advanced opt-in mode requiring authentication and an explicit
warning; it is not part of the MVP.

## 11. Scheduling and process model

`wikisyncd` owns scheduled synchronization and background local transformations.
Support:

- Interval and daily schedules with jitter.
- Manual pause and metered-network avoidance where the OS exposes it reliably.
- Per-run bandwidth and concurrency limits.
- Sleep/wake recovery without duplicate jobs.
- Graceful shutdown with resumable checkpoints.

Install as a `launchd` user agent on macOS and a `systemd --user` service/timer on
Linux. During early development, `wikisync daemon --foreground` provides identical
behavior without system installation.

## 12. Security and privacy

Create a threat model before beta covering malicious upstream content, corrupted API
responses, hostile HTML/media, local unprivileged users, exposed web listeners,
dependency compromise, and rollback/tampering with the archive.

Required controls:

- No telemetry or external assets.
- Explicit source allowlist and HTTPS through Rustls.
- Application-specific User-Agent with operator contact configuration.
- Bounded response sizes, timeouts, decompression ratios, parser work, and redirects.
- Sanitized HTML and restrictive CSP.
- Data-directory permissions restricted to the user.
- BLAKE3 identities for all canonical content objects and media.
- Append-only, predecessor-linked JSON manifests describing each committed sync.
- Optional Ed25519 signing; allow the trusted manifest head/public key to be exported
  to separate storage.
- `verify` checks loose and packed objects, delta reconstruction, pack indexes,
  revision chains, manifests, cache-version consistency, and search pointers.
- Signed release artifacts and locked/audited Rust dependencies before stable v1.

Full-disk encryption is the recommended at-rest protection for v1. Searchable
application-managed encryption is deferred until its key management, crash recovery,
and performance properties can be implemented without giving users a false security
claim.

## 13. Licensing and attribution

- Preserve source wiki, canonical page/revision URLs, revision authorship metadata,
  and applicable text-license notices.
- The local reader and Markdown exports include an attribution/source section.
- Image capture is disabled until per-file attribution and licensing metadata are
  stored and displayed correctly.
- Do not present the application as an official Wikimedia product or misuse Wikimedia
  trademarks.
- Include a clear notice that deleted/suppressed material retained locally may carry
  legal and privacy obligations.

## 14. Observability and diagnostics

Use structured local logs with automatic removal or hashing of query text where it is
not needed. Record counters for API calls, downloaded bytes, revisions captured,
backoff time, local transform failures, index lag, and verification results.

`wikisync doctor` reports configuration, database version, free space, lock/daemon
state, source reachability when online, incomplete jobs, and integrity status. It must
offer a redacted diagnostic bundle suitable for a bug report.

## 15. Test strategy

### Unit tests

- Selection and history policies.
- API continuation and retry decisions.
- Revision-chain gap detection.
- Wikitext normalization and deterministic Markdown fixtures.
- Exact and reading diffs.
- Content-object hashing, compression, and reconstruction.
- Logical object identity across loose, packed-full, and packed-delta representations.
- Delta-base selection limits and maximum reconstruction depth.
- Manifest chaining and verification.

### Integration tests

- Recorded MediaWiki responses for pages, redirects, moves, deletions, categories,
  pagination, throttling, malformed data, and missing revisions.
- Temporary SQLite libraries across every migration.
- Crash points between loose-object write, metadata commit, index update, and
  checkpoint.
- Crash points during pack creation, verification, activation, and obsolete-object
  cleanup.
- Repacking an object graph preserves every object ID, revision lookup, and manifest.
- Daemon/CLI writer exclusion and concurrent readers.

### End-to-end tests

- Bootstrap a fixture collection, disable networking, then read, search, navigate,
  inspect history, diff, and export it.
- Introduce multiple upstream revisions and prove every intermediate revision arrives.
- Simulate a gap beyond RecentChanges retention and prove reconciliation succeeds.
- Tamper with an object, database pointer, pack index, and manifest and prove
  verification fails.
- Pack and repack a multi-revision library, remove the superseded representations, and
  prove every revision reconstructs to the same canonical hash.
- Crawl the local reader while blocking all outbound traffic and prove it makes no
  external requests.
- Interrupt sync repeatedly and prove the final archive equals an uninterrupted run.

Run CI on current stable Rust for macOS and Ubuntu. Add property/fuzz tests for parsers,
compressed objects and packs, and untrusted API inputs before beta.

## 16. Milestones and gates

### Milestone 0: architecture spike

Deliver:

- Cargo workspace skeleton and architectural decision records.
- MediaWiki client spike for 100 pages.
- Loose content-object store, revision graph, simple document conversion, FTS search,
  and diff prototype.
- API/disk benchmarks and a written renderer evaluation.

Gate: after disconnecting the network, the prototype can read, search, inspect history,
and diff its captured pages. No unresolved feasibility risk remains for the canonical
revision path.

### Milestone 1: CLI vertical slice

Deliver:

- Explicit-title collections.
- Current plus future revision sync.
- Transactional SQLite/loose-object storage and migrations.
- CLI search/show/history/diff/status/export.
- Minimal local website.

Gate: normal updates contact Wikimedia only for discovery and selected changed pages;
sync is idempotent and resumes after termination.

### Milestone 2: history and recovery

Deliver:

- Last-N, since-date, and complete selected-page history policies.
- RecentChanges overlap ingestion.
- Long-gap reconciliation.
- Moves, redirects, deletions, and restorations.
- Integrity manifests and full verification.
- Restartable pack creation with complete and bounded-delta entries.

Gate: fixture tests prove no intermediate public revision is missed across routine and
long-gap scenarios. Packing and repacking preserve the content ID and reconstructed
bytes of every revision.

### Milestone 3: collections and GUI

Deliver:

- Category recursion and title-list import.
- Collection estimates, budgets, and retention policies.
- Iced onboarding, collection editor, sync dashboard, and integrity UI.
- Production-quality local reader typography, references, tables, and navigation.

Gate: a nontechnical user can create, sync, browse, and update a collection without
using the CLI.

### Milestone 4: daemon, security, and beta packaging

Deliver:

- Single-writer daemon and scheduling.
- `launchd` and `systemd --user` integration.
- Threat-model controls and outbound-network test suite.
- Signed macOS and Linux beta packages.
- Migration, backup, restore, and diagnostic documentation.

Gate: scheduled sync survives sleep, restart, throttling, cancellation, and partial
failures without archive corruption or revision loss.

### Milestone 5: stable v1

Deliver:

- Multi-language collections.
- Optional thumbnail images with captions, attribution, and budgets.
- Current-language dump bootstrap.
- Pack heuristics and storage tuning based on beta libraries.
- Stable configuration, database, CLI JSON, export, and backup contracts.

Gate: all release acceptance criteria below pass on macOS and Ubuntu, and an older beta
library upgrades without data loss.

### Post-v1 investigations

- Cross-page and cross-pack delta selection beyond the conservative v1 heuristics.
- Tantivy for very large libraries.
- Higher-resolution media policies.
- Additional MediaWiki projects and third-party MediaWiki instances.
- Reproducible dependency-aware rendering as a separate advanced mode.
- Windows service and installer support.

## 17. Release acceptance criteria

A stable release is acceptable when:

- A synchronized collection is fully readable and searchable without networking.
- The local website performs no external requests in default mode.
- Routine sync downloads content only for newly selected pages and missing revisions
  of changed selected pages.
- Every captured revision is immutable, attributable, and arbitrarily diffable.
- Repacking changes no content object ID, revision identity, or integrity manifest.
- A sync interrupted at every tested crash point resumes without corruption or loss.
- A gap longer than RecentChanges retention is reconciled.
- Moves and deletion do not erase captured local history.
- Tampering with canonical content or manifest history is detected.
- Derived Markdown can be rebuilt without network access.
- Current collections export as clean, provenance-bearing Markdown/plain text.
- The GUI exposes scope, expected storage, history, media, schedule, and removal policy
  before starting a large operation.
- Default web serving is loopback-only and read-only.
- Text and captured images display their required source and licensing information.
- Documentation accurately explains bandwidth behavior, storage growth, privacy risks,
  and the boundary between integrity and truth.

## 18. First implementation backlog

Execute these items in order after Milestone 0 decisions are recorded:

1. Initialize the Cargo workspace, formatting, linting, CI, and license policy.
2. Define `WikiId`, `PageId`, `RevisionId`, collection rules, and history policies.
3. Implement the bounded MediaWiki HTTP client and fixture harness.
4. Create SQLite migration 1 and the atomic loose content-object store, with physical
   location isolated from logical content identity.
5. Implement explicit-title resolution and current-revision capture.
6. Add normalized plain text, minimal Markdown, and deterministic transformer versions.
7. Add FTS indexing and CLI search/show.
8. Add revision enumeration, history, and local diff.
9. Add resumable sync jobs, overlap checkpoints, and structured status.
10. Add the minimal Axum reader and outbound-network test.
11. Prove the complete offline vertical slice.
12. Implement verified, restartable packfiles and bounded delta reconstruction.
13. Only then begin category recursion, long-gap recovery, and the Iced GUI.

## 19. Reference constraints

- MediaWiki revision API: <https://www.mediawiki.org/wiki/API:Revisions/en>
- MediaWiki RecentChanges behavior and retention:
  <https://www.mediawiki.org/wiki/API:RecentChanges>
- Wikimedia dump types and formats:
  <https://meta.wikimedia.org/wiki/Data_dumps/What%27s_available_for_download>
- Wikimedia dump scale guidance:
  <https://meta.wikimedia.org/wiki/Data_dumps/Dumps_sizes_and_growth>
- Wikimedia API usage guidelines:
  <https://foundation.wikimedia.org/wiki/Policy:Wikimedia_Foundation_API_Usage_Guidelines>
- MediaWiki `maxlag` guidance:
  <https://www.mediawiki.org/wiki/Manual:Maxlag_parameter>
- Wikimedia licensing and attribution terms:
  <https://foundation.wikimedia.org/wiki/Policy:Terms_of_Use>
