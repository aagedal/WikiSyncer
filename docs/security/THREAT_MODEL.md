# WikiSyncer threat model

Status: pre-beta security baseline

Last reviewed against the repository: 2026-08-21

This document describes the security and privacy properties WikiSyncer intends to
provide, the controls present at the review date, and the work that remains before a
beta can claim the Milestone 4 security deliverable is complete. It covers the local
library, synchronization path, derived-content pipeline, reader, daemon boundary,
dependencies, and release artifacts.

This is a living threat model. Re-review it after changes to network sources,
rendering, media support, daemon IPC, manifests, backup/restore, or packaging.

## Security promise and non-claims

WikiSyncer preserves three distinct classes of information:

1. **Canonical evidence**: exact public wikitext and revision metadata returned by a
   configured MediaWiki source.
2. **Derived content**: normalized text, Markdown, rendered HTML, diffs, and search
   data that can be rebuilt from canonical evidence.
3. **Local provenance**: the source endpoint, capture observations, content hashes,
   synchronization state, manifests, and software version.

Integrity verification can establish that captured bytes still match a recorded
content identity, and—once authenticated manifests exist—that an observed archive
state descends from a separately trusted manifest head. It cannot establish that
upstream content was accurate, unbiased, complete, lawful, safe, or free from
upstream manipulation. SHA-1 metadata from MediaWiki is a source-consistency check,
not a modern authenticity proof. A local BLAKE3 object identity is evidence about
bytes, not objective truth.

User-facing language must therefore say **captured**, **observed**, or **verified
since capture**. It must not call archived statements true merely because their bytes
or manifest chain verify.

WikiSyncer is not currently designed to resist an administrator, root user, malware
running as the library owner, or an attacker who can replace both the application and
all trusted state. Full-disk encryption is the recommended at-rest confidentiality
control for v1. Application-managed searchable encryption is out of scope until its
key management and recovery design can support honest security claims.

## Protected assets

| Asset | Required property | Consequence of loss |
| --- | --- | --- |
| Canonical revision bytes | Integrity, durability, correct source association | Historical evidence is corrupted, lost, or attributed to the wrong revision |
| Revision/page graph and source identity | Integrity, stable identity, completeness within the selected policy | Revisions can be hidden, reordered, spliced between pages, or presented under a false source |
| Manifest history and trusted head | Append-only continuity, authenticity when signed, rollback resistance | A coherent older or attacker-created library can appear current |
| Collection rules, budgets, and checkpoints | Integrity and durability | Unwanted content is fetched, history is skipped, or resource limits are bypassed |
| Search and derived representations | Rebuildability and safe presentation | Users see misleading, stale, or executable output; canonical evidence must remain unaffected |
| Library confidentiality | Access limited to the owning account unless explicitly shared | Reading interests, retained suppressed material, authors, comments, and content are disclosed |
| Daemon/write authority | Single-writer authorization and request integrity | Local clients race, corrupt state, or cause unauthorized synchronization |
| Release and dependency chain | Authenticity and reproducibility sufficient for the release claim | A compromised binary can bypass every in-application control |
| Availability and resource budgets | Bounded CPU, memory, disk, and network work | Malicious or pathological input makes synchronization or reading unusable |

Revision author names, IP addresses, edit comments, selected titles, search queries,
and retained deleted or suppressed material can be sensitive even when originally
public. Diagnostic output and backups must be treated as library data, not harmless
operational metadata.

## Actors and assumptions

The model considers:

- a malicious or compromised MediaWiki source that can return false, inconsistent,
  adversarial, oversized, or specially structured content and metadata;
- an on-path attacker, compromised proxy, or corrupted transport response;
- hostile wikitext, derived Markdown/HTML, links, and future media files;
- another unprivileged user on the same computer;
- untrusted local web content attempting to reach the reader through a browser;
- an accidental or deliberate non-loopback listener;
- a local attacker with write access to a copied, mispermissioned, restored, or
  otherwise exposed library;
- a compromised crate, build tool, CI action, signing system, or release host; and
- ordinary crashes, cancellation, partial writes, bit rot, and stale backups that can
  resemble malicious tampering.

The operating-system kernel, filesystem durability primitives, user-account boundary,
TLS implementation, and a separately stored trusted manifest head/public key are
trusted when their corresponding guarantee is claimed. The configured upstream is
trusted only as the origin of an observation, never as an oracle of truth.

## Trust boundaries and data flow

```text
untrusted MediaWiki / network
        |
        | HTTPS request, bounded JSON response
        v
MediaWiki adapter -- identity/size/model/SHA-1 checks --> sync planner
        |                                                   |
        | canonical bytes                                   | durable jobs/checkpoints
        v                                                   v
private loose-object staging --> content-addressed store + SQLite metadata
                                      |               |
                                      | verified read | transaction/WAL
                                      v               v
                               derived text/search/HTML
                                      |
                                      | read-only HTTP on loopback
                                      v
                              local browser / local users

GUI and CLI -- bounded local IPC/direct lease --> single-writer daemon or short writer

build inputs / crates / CI --> release pipeline --> installed executable (pre-beta trust root)
```

Important boundary notes:

- TLS authenticates a configured network endpoint according to the platform/root
  certificate trust model. It does not make the endpoint's statements true.
- The SQLite database is metadata authority today, but it is not itself
  cryptographically authenticated. Content-addressed reads catch disagreement with
  object IDs; they do not catch a coherent rewrite or rollback of database rows and
  objects.
- Derived data is untrusted output of an untrusted input pipeline. It is disposable
  and must never become a second canonical source.
- Loopback prevents remote network exposure, but it is not an operating-system user
  boundary. Another local user or process may connect to an unauthenticated localhost
  port even when it cannot read the `0700` library directory directly.
- The daemon IPC endpoint and client authentication/authorization rules become a new
  high-value trust boundary. They must be reviewed before unattended operation is a
  beta feature.

## Controls verified in the current implementation

The following statements are based on inspection of the named code and tests. They
describe present behavior, not all controls required by the plan.

### Network ingestion

- `wikisync-mediawiki::ClientConfig` requires HTTPS for non-loopback endpoints;
  credentials, query strings, and fragments in configured endpoints are rejected.
  Plain HTTP is accepted only for loopback fixture servers.
- `reqwest` is built without default features and with `rustls-tls`. The client has
  request and connect timeouts, a three-redirect limit, a fixed non-empty User-Agent,
  `maxlag`, bounded request batch sizes, and an 8 MiB default response-body limit
  enforced both from `Content-Length` and while consuming chunks.
- Pagination is explicit and one bounded response is processed at a time. Retry
  classification is conservative and exposes `Retry-After`; malformed identities and
  invalid responses are not classified as retryable.
- Revision-content requests bind the expected page and revision IDs. Capture checks
  immutable metadata consistency, the `wikitext` content model, declared size when
  present, UTF-8, and MediaWiki's base-36 SHA-1 when present before committing.
- Current and history capture use stable page/revision IDs rather than titles as
  identity. Long-gap reconciliation checks parent continuity and bounds batches and
  revisions per page.

These controls reduce corruption, confusion, and resource-exhaustion risk. They do
not yet constitute the required explicit source allowlist: arbitrary HTTPS hosts are
accepted, and redirect destinations are count/scheme constrained but not pinned to an
approved host set.

### Local storage and integrity

- Object identity is domain-separated BLAKE3 over kind, canonical length, and bytes.
  Revision rows refer to logical IDs rather than filesystem locations.
- New loose objects have a configured 64 MiB uncompressed default limit, are streamed
  through bounded buffers, compressed to a temporary file, synchronized, atomically
  installed, and recorded in SQLite only after installation.
- Library directories are restricted to `0700` and the SQLite database/WAL/SHM files
  to `0600` on Unix. A symbolic-link database path is rejected. SQLite enables foreign
  keys, WAL, `synchronous=FULL`, and a bounded busy timeout.
- Loose-object reads reconstruct bytes and verify the expected kind, length, and
  BLAKE3 identity.
- Packs and their separate indexes are checksummed and verified before activation.
  Delta depth, pack object count, pack input bytes, offsets, base ordering, and
  reconstruction are bounded/validated. Existing representations remain available
  until replacements verify; pruning and pack retirement require a verified alternate
  representation.
- `wikisync-integrity` performs bounded quick object scans or complete logical-object
  and predecessor-manifest scans and reports partial coverage honestly. Full scans
  validate canonical manifest identity, sequence/link continuity, and coverage of
  manifest-eligible successful runs. A complete stable scan with no findings is
  labeled only as "verified since capture," not as truth.
- Tests exercise tampered loose objects, pack payloads, indexes, and database location
  pointers.

Current full verification covers the object catalog and unsigned predecessor-linked
manifest history. It does not yet authenticate a signing identity or fully verify
revision chains, cache transformer versions, search pointers, or an externally
trusted library head.

### Derived content, HTML, and reader

- Canonical wikitext is retained unchanged; normalized text and Markdown have explicit
  transformer versions and are rebuildable.
- The transformer interprets a conservative subset, strips/ignores raw tags instead
  of executing them, turns unknown templates into visible placeholders, and caps
  recursive inline processing at 32 levels.
- The reader converts any raw Markdown HTML events to text, rewrites local article
  links to local routes, rejects non-HTTP schemes by rewriting them to `#`, and
  replaces all article image destinations with an inert embedded one-pixel image.
  External HTTP(S) links can remain clickable but are not loaded as page assets.
- Every response applies a restrictive Content Security Policy: no default loads, no
  scripts, connections, fonts, or frames; styles and forms are same-origin; images
  are same-origin or `data:`. Responses also use `nosniff`, `no-referrer`, and
  `no-store`.
- The Axum router exposes GET-only reading routes and bundled CSS. Serving rejects
  non-loopback addresses; the GUI helper binds an ephemeral IPv4 loopback port and
  has graceful shutdown ownership.
- In-process crawl tests enumerate loaded resources and prove they remain local or
  embedded. Tests also cover hostile-looking content, header policy, non-loopback
  rejection, and listener shutdown.

Media capture is not implemented and should remain disabled until MIME validation,
byte/decompression/pixel limits, active-format handling, attribution, and safe serving
are implemented and tested.

### Dependency and build policy

- Workspace code forbids `unsafe` and declares a stable minimum Rust version.
- `Cargo.lock` is committed. CI runs formatting, Clippy with warnings denied, and
  locked workspace tests on macOS and Ubuntu.
- `cargo-deny` runs in CI with explicit license policy, denied wildcard dependencies,
  denied unknown registries/Git sources, and yanked-crate denial.
- Three unmaintained-only advisories in Iced 0.13's transitive timer and text stacks
  have narrow documented exceptions in `deny.toml` because the advisories offer no
  safe compatible upgrade. Re-evaluate and remove them with the next Iced upgrade.
- The HTTP stack uses Rustls rather than a platform OpenSSL dependency.

This is useful hygiene, not a complete software-supply-chain guarantee. Release
signing, reproducible builds, action pinning, dependency review/audit response, an
SBOM/provenance statement, and secured signing keys remain release work.

## Abuse cases and required treatment

### Malicious upstream statements and metadata

**Scenario.** An upstream editor or compromised wiki publishes false or defamatory
text, misleading citations, confusable titles, poisoned prompts for local AI tools,
or metadata crafted to misattribute a revision.

**Present resistance.** Wiki/page/revision identity and immutable metadata are checked;
the exact source is retained separately from derived views; provenance and integrity
language avoids a truth claim. Conservative transformation prevents templates or raw
HTML from gaining execution authority.

**Residual risk and action.** Validly served malicious text is expected archive data
and will be searchable/exportable. UI/export documentation must preserve source,
revision, capture time, and the integrity-versus-truth warning. AI exports should be
treated as untrusted data and must not be presented as instructions. Unicode
confusable and bidirectional-control handling needs explicit display tests and, where
needed, visible warnings without rewriting canonical bytes.

### Corrupted, inconsistent, or resource-exhausting API responses

**Scenario.** A server returns oversized or endless bodies, malformed JSON,
inconsistent page/revision IDs, wrong sizes/hashes/parents, redirect loops, slow
responses, decompression bombs, or pathological continuation tokens.

**Present resistance.** Transport timeouts, response/batch/operation limits, limited
redirects, explicit continuations, schema decoding, identity checks, UTF-8/content
model/size/SHA-1 validation, durable job state, history budgets, and long-gap limits
fail closed for the implemented paths.

**Residual risk and action.** Enforce an approved host/source policy across initial
URLs, DNS resolution as appropriate, and every redirect. Add explicit compressed-byte
and decompression-ratio tests if content encoding is enabled. Add total operation,
parser-output, allocation, and disk-growth budgets independent of server metadata.
Fuzz JSON/schema adapters, continuation handling, the wikitext transformer, pack
decoding, and delta reconstruction. Exercise slow-loris, truncation, redirect, and
retry storms in offline fixtures.

### Hostile HTML, links, and media

**Scenario.** Wikitext attempts script execution, attribute injection, unsafe URL
schemes, remote tracking loads, CSS abuse, active SVG, decompression bombs, huge image
dimensions, or misleading link destinations.

**Present resistance.** Raw tags do not pass through as active article HTML; generated
values are escaped or emitted through the Markdown event renderer; URL schemes are
restricted; article images become inert placeholders; CSP blocks scripts, network
connections, remote styles/fonts, and framing; reader assets are bundled.

**Residual risk and action.** Keep media disabled for beta unless a separate bounded
pipeline validates MIME by bytes, rejects or safely rasterizes active formats, caps
compressed bytes, decoded bytes, dimensions, frame count, and CPU, and serves content
with fixed safe types plus `nosniff`. Add corpus and property tests for markup/link
edge cases. External links are an intentional user-initiated escape from offline mode
and must be visually identifiable; consider an interstitial or opt-in policy where
privacy expectations require it.

### Local unprivileged users and filesystem manipulation

**Scenario.** Another OS user reads the archive, changes objects/metadata, substitutes
symlinks, exhausts disk space, or restores a stale coherent copy.

**Present resistance.** Unix library/database permissions, symlink rejection for the
database, content-addressed verified reads, transactional SQLite, durable file
installation, collection budgets, and pack/pointer checks protect the normal private
library layout.

**Residual risk and action.** Verify permissions for every created artifact,
including manifests, exports, backups, logs, IPC sockets, package/service files, and
temporary files. Add adversarial tests for pre-existing symlinks and path replacement
throughout the object/pack/backup paths. Treat disk exhaustion and filesystem errors
as resumable failures. A user who obtains write access can coherently rewrite or roll
back the unauthenticated database and object set; authenticated manifests plus a
separately stored trusted head are required to detect that class of attack.

### Exposed reader or daemon listener

**Scenario.** A configuration error binds the reader to a LAN interface; hostile web
content probes localhost; another local user scans a predictable port; or an
unauthorized client sends daemon mutation requests.

**Present resistance.** Reader binding is loopback-only, routes are read-only, CSP
blocks cross-origin connection behavior from reader pages, and the GUI uses an
ephemeral listener with graceful shutdown. No reader mutation route exists, so blind
cross-site GETs cannot modify the archive.

**Residual risk and action.** Loopback alone does not provide confidentiality between
local users. Before beta, either authenticate reader requests with an unguessable,
short-lived capability, use an OS-user-bound transport/proxy, or explicitly declare
and document the local-user disclosure limitation after a security review. Validate
Host/origin behavior where relevant and add browser tests for cross-origin reads and
requests. LAN serving remains disabled until it has authentication and an explicit
warning. Daemon IPC is library-local, owner-only by directory/socket permissions,
bounded and versioned, with no network listener; the GUI and CLI now use its
cooperative writer-access API for implemented mutations. Peer-credential review,
replay/idempotency rules, signal-safe shutdown, stale-socket recovery, and broader
adversarial forwarding tests remain beta work.

### Dependency, CI, and release compromise

**Scenario.** A crate, registry account, CI action, compiler/tool, build worker,
updater, or signing key supplies a malicious executable.

**Present resistance.** Locked dependencies, constrained registries/sources and
licenses, yanked dependency checks, warnings-as-errors, two-platform CI, Rustls, and
forbidden workspace `unsafe` reduce accidental and some supply-chain risk.

**Residual risk and action.** Define vulnerability severity/response policy and run
advisory checks on every release. Review new dependencies, minimize features, pin CI
actions by immutable digest/commit, protect release environments and signing keys,
produce checksums/SBOM/provenance, and sign macOS and Linux artifacts. Rebuild or
otherwise independently verify release artifacts before publication. A valid package
signature proves origin under the signing-key policy, not absence of malicious code.

### Archive tampering and rollback

**Scenario.** An attacker alters loose bytes, a pack/index, SQLite pointers, revision
links, page heads, checkpoints, or search data; deletes recent revisions; swaps in an
older complete backup; or creates a new self-consistent object graph.

**Present resistance.** Object and pack identities, verified reconstruction, database
constraints, immutable metadata conflict checks, transactional checkpoints, durable
jobs, canonical predecessor-linked manifests, completed-run coverage checks, and
focused tamper tests catch many partial or accidental changes.

**Residual risk and action.** Extend full verification from implemented manifest
canonicalization, chain continuity, and completed-run coverage to revision/page
linkage, object reachability, checkpoints, transformer/search pointers, and absence
of unexpected database truncation. Optional Ed25519 signing must be domain separated
and define key rotation/revocation.
Export the trusted public key and latest manifest identity to separate storage. Without
that external anchor, a complete rollback can only be reported as internally
self-consistent, not current. Backup/restore must preserve and compare anchors and
must never silently reset trust.

### Privacy retention and diagnostics

**Scenario.** WikiSyncer retains revisions later deleted or suppressed for privacy,
copyright, or safety; logs, exports, backups, crash reports, or query telemetry reveal
reading interests or personal data.

**Present resistance.** Captured history is retained deliberately, the reader sends no
telemetry or external asset requests in its tested default mode, responses use
`no-referrer`, and library files are private on Unix. The offline `doctor` bundle uses
a tested field allowlist, aggregate counts, bounded quick verification, create-new
`0600` output, and redacted section error codes; sentinel tests reject endpoints,
titles, names, paths, raw errors, content, object IDs, and environment values.

**Residual risk and action.** Onboarding, export, backup, and restore documentation
must warn about retained suppressed material and legal obligations. Diagnostic
redaction must remain an explicit versioned allowlist, and users must review bundles
before sharing them. Define bounded log retention and verify no telemetry endpoints
or unexpected outbound requests in release binaries. Purge, if later implemented, must
be a separately authorized destructive workflow with preview and clear limits on
recoverability from packs/backups.

## Present versus pending control register

| Requirement | State at review | Evidence / remaining work |
| --- | --- | --- |
| No telemetry or external reader assets | **Present for tested reader path** | Bundled CSS, restrictive CSP, and in-process outbound-resource crawl; add release-level outbound test and audit diagnostics/updater behavior |
| Explicit source allowlist | **Present for configured source origin** | Each client derives an explicit normalized host allowlist from its selected endpoint; redirects must remain on the exact scheme/host/effective-port origin and cross-origin destinations fail before contact. Private-address/DNS-rebinding policy remains to be reviewed |
| HTTPS through Rustls | **Present** | `wikisync-mediawiki` disables reqwest defaults and enables `rustls-tls`; loopback HTTP is fixture-only by validation |
| Application User-Agent and operator contact | **Partial** | Non-empty controlled User-Agent and `maxlag` exist; configuration/UI contract for operator contact remains |
| Response, timeout, parser, redirect, and decompression bounds | **Partial** | Request/connect timeout, per-response and aggregate run-byte limits, clone-shared concurrency, redirect count/origin policy, object/pack and inline-depth limits exist; byte-rate shaping, decompression-ratio coverage, broader parser/allocation budgets, and fuzzing remain |
| Sanitized HTML and restrictive CSP | **Present for text reader** | Raw HTML events become text, links/images are rewritten, headers are tested; continue adversarial corpus testing |
| User-restricted data-directory permissions | **Present on Unix core paths** | Directories including manifests/exports are `0700`; SQLite/WAL/SHM and created manifest/export files are `0600`; extend release auditing to logs, IPC, and backups |
| BLAKE3 identities for canonical text/media | **Present for object abstraction** | Domain-separated text/media object kinds and verified reads exist; media ingestion is not implemented |
| Predecessor-linked manifests | **Present, unsigned** | Bounded canonical JSON, BLAKE3 body identity, immutable run configuration snapshots, strict predecessor/sequence checks, atomic durable append, bounded crash-gap repair, catalog-difference revisions, resulting heads, and tamper tests are implemented |
| Optional Ed25519 signatures and external trust anchor | **Pending** | No signing/key lifecycle or trusted-head export is present |
| Full verify contract | **Partial** | Loose/pack/delta/object catalog plus unsigned manifest identity/chain/run coverage verification exists; revision/page/checkpoint reachability, search/cache pointers, database truncation, signatures, and trusted-head comparison remain |
| Loopback-only read-only reader | **Present, confidentiality gap** | Non-loopback addresses are rejected and only GET routes exist; localhost is unauthenticated and not an OS-user boundary |
| Daemon single-writer authorization | **Partial** | Versioned bounded Unix IPC, `0600` sockets inside the private library, cooperative writer leases, scheduling, signal cancellation, advisory-lock stale-socket recovery, GUI/CLI forwarding, and exclusion/recovery tests exist; peer-credential review and hostile same-UID analysis remain |
| Locked/audited dependencies | **Partial** | Lockfile, cargo-deny policy, locked CI tests; formal audit response, immutable CI action pins, SBOM/provenance remain |
| Signed beta packages | **Pending** | Packaging, platform signing, protected key flow, and artifact verification remain |
| Safe optional media | **Deferred / disabled** | Image placeholders only; do not enable capture until bounded validation, attribution, licensing, and safe serving land |
| At-rest confidentiality | **External control** | Recommend full-disk encryption; application-managed encryption is intentionally deferred |

## Beta security exit checks

The threat-model portion of Milestone 4 should not be marked complete until every
applicable check below has an owner, automated or recorded evidence, and a passing
result on macOS and Ubuntu. Deferred items must be disabled and accurately documented.

1. **Trust language:** GUI, CLI, reader, exports, verification reports, and docs use
   captured/observed/verified-since-capture language and explain that integrity is not
   truth or continued upstream availability.
2. **Source policy:** configured endpoints and every redirect are constrained to the
   explicit approved source policy; loopback HTTP remains test-only; SSRF/private
   address and DNS/redirect cases have tests.
3. **Outbound behavior:** an end-to-end release-mode test blocks or records networking
   and proves offline reading, search, diff, verification, GUI viewing, and local
   transformations make no outbound requests or external asset loads. Sync reaches
   only the approved source endpoints.
4. **Untrusted response bounds:** fixtures cover oversized/truncated/slow/malformed
   JSON, invalid identities and parents, continuation loops, retry storms, redirects,
   compressed-body limits where applicable, and cancellation. Failures do not advance
   checkpoints beyond durable canonical content.
5. **Parser and storage robustness:** fuzz/property suites exercise wikitext, Markdown
   event rewriting, JSON adapters, loose decompression, pack/index decoding, and delta
   reconstruction under explicit CPU/memory/output/depth/ratio limits.
6. **Hostile presentation:** a security corpus proves scripts, raw HTML, unsafe URL
   schemes, attributes, images, SVG, CSS tricks, and remote loads cannot execute or
   load automatically. CSP and response headers are asserted for success and error
   pages.
7. **Listener exposure:** reader non-loopback rejection is retained; the multi-user
   localhost confidentiality decision is implemented and tested. LAN mode stays
   unavailable unless authentication and warnings pass review. Daemon IPC is
   owner-only, authenticated/peer-checked, bounded, and has no network exposure.
8. **Private local artifacts:** clean and adversarial-permission tests verify library,
   database/WAL/SHM, objects, packs, manifests, IPC, logs, exports, backups, and temp
   files. Symlink/path-swap cases fail safely.
9. **Complete verification:** full verification detects object, pack, index, database
   pointer, revision-chain, manifest-chain, page-head, checkpoint, transformer/cache,
   and search-pointer tampering, as well as deletion/truncation where a trusted head
   makes it observable. Quick verification never implies complete coverage.
10. **Rollback resistance:** predecessor-linked manifests are append-only and
    deterministically encoded; optional Ed25519 signatures and key lifecycle are
    tested; separately exported trusted head/public-key comparison detects restoration
    of an older coherent library. Backup/restore preserves this contract.
11. **Crash and writer safety:** daemon/GUI/CLI exclusion, concurrent readers,
    sleep/wake, restart, throttling, cancellation, partial writes, disk-full behavior,
    pack activation, and service shutdown pass fixture-backed tests without revision
    loss or archive corruption.
12. **Supply chain and packages:** dependencies pass the defined advisory/license/
    source policy from the lockfile; release inputs and CI actions are immutably pinned
    or equivalently controlled; macOS and Linux packages are signed and their
    checksums/provenance are verified through a documented clean install/upgrade.
13. **Privacy documentation:** onboarding and release docs cover bandwidth, storage
    growth, external-link behavior, local-listener exposure, full-disk encryption,
    logs/diagnostics, backups, and retention of material later removed upstream.
14. **Media decision:** media remains disabled, or its bounded decode/rasterization,
    MIME handling, CSP, source/license/attribution, budget, and malicious corpus tests
    all pass. Text synchronization must remain usable when media fails.

## Review triggers

Repeat the threat analysis when any of these changes occur:

- a new source type, dump importer, redirect policy, proxy, authentication mechanism,
  updater, telemetry, or network dependency;
- daemon IPC or service installation changes;
- media ingestion or a new HTML/Markdown renderer;
- manifest format, signature scheme, key storage, backup, restore, migration, export,
  or purge behavior;
- non-loopback serving or collaborative/multi-user access;
- a new object/pack encoding, database identity relationship, or verification claim;
  or
- a release/signing/CI pipeline change or a material dependency advisory.

Security-sensitive changes should update this control register and add fixture-backed
tests. A check should be described as complete only when the implementation and its
failure-path tests support the claim.
