# WikiSyncer threat model

Status: pre-beta security baseline

Last reviewed against the repository: 2026-08-25

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
- hostile wikitext, derived Markdown/HTML, links, and captured media files;
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
  high-value trust boundary. On macOS and Linux, the daemon asks the kernel for each
  accepted Unix-socket peer's effective UID and drops the connection before reading a
  request unless it matches the daemon's effective UID. Socket-path permissions are
  defense in depth rather than the identity check.
- Same-UID local processes remain inside the daemon trust boundary. Any process
  running as the library owner can connect and submit permitted daemon operations;
  peer credentials do not distinguish benign applications from malware or a
  compromised process under that account. Root and administrators remain outside the
  threat model, as stated above.

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
- Each client disables ambient HTTP proxies and installs a source-bound DNS resolver.
  Literal endpoint addresses are checked during configuration. For hostnames, every
  resolution is limited to 32 addresses and the complete answer is rejected if it is
  empty or contains a private, loopback, link-local, multicast, documentation,
  reserved, or unsafe IPv4-transition destination. Only the vetted socket-address
  iterator reaches `reqwest`'s connector. The explicit loopback-HTTP fixture policy
  accepts only loopback answers and cannot be enabled for an HTTPS source.
- Redirects must remain on the configured scheme, normalized host, and effective port.
  A cross-origin redirect is rejected before destination resolution or contact. A
  same-origin redirect remains behind the same DNS policy. A pooled connection may be
  reused without a fresh lookup, but it stays connected to the address that was vetted
  when that socket was created; every new connection resolves and validates again.
- Pagination is explicit and one bounded response is processed at a time. Retry
  classification is conservative and exposes `Retry-After`; malformed identities and
  invalid responses are not classified as retryable.
- Revision-content requests bind the expected page and revision IDs. Capture checks
  immutable metadata consistency, the `wikitext` content model, declared size when
  present, UTF-8, and MediaWiki's base-36 SHA-1 when present before committing.
- Current and history capture use stable page/revision IDs rather than titles as
  identity. Long-gap reconciliation checks parent continuity and bounds batches and
  revisions per page.

These controls reduce corruption, confusion, source-confusion, DNS-rebinding, SSRF,
and resource-exhaustion risk for the direct configured-source transport. Configuring
an arbitrary public HTTPS source is still an explicit user trust decision; the policy
does not claim that a public destination or its content is benign.

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

Current full verification covers the object catalog, predecessor-linked manifest
history, and bounded metadata reachability for revisions, page heads, checkpoints,
search documents, and contentless FTS rows. It also checks the persisted search
transformer version. The CLI and GUI can generate or validate an external PKCS#8
Ed25519 key, sign a validated manifest-chain head, publish a canonical external
trusted-head document only after authenticated full verification, and compare that
anchor with a later full verification. The CLI additionally supports create-new key
import and a rotation flow that retains the previous anchor before replacing the
current one. The CLI lifecycle enforces explicit absolute paths outside the library,
private owned parent directories, bounded regular-file reads, create-new secret
writes, and atomic anchor refresh. The current schema has no derived-cache inventory,
and contentless FTS permits pointer checks rather than reconstruction of its indexed
token stream.

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

Optional thumbnail capture is implemented and remains default-off. The supported path
accepts only bounded passive JPEG/PNG renditions whose MIME type, structure, decoded
dimensions, pixel count, animation state, and allocation requirements validate. It
stores immutable attribution, licensing, source, rendition, placement, and capture
metadata; authenticates media inventory and placements in current manifest schemas;
revalidates local bytes during full verification, reader serving, and export; and
treats per-image source failures as nonfatal to already durable text. Active formats,
higher-resolution policies, and unbounded media remain unsupported.

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
- A credential-free native-runner workflow builds bounded, deterministic candidate
  archives, verifies canonical layouts and SHA-256 manifests, and pins its checkout
  action by commit. Local tooling can create and verify a detached OpenSSH Ed25519
  signature over the checksum manifest without copying the private key into output.

This is useful hygiene, not a complete software-supply-chain guarantee. Release
platform signing/notarization, binary reproducibility, remaining action pinning,
dependency review/audit response, an SBOM/provenance statement, protected signing
identities, and trust-anchor distribution remain release work.

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
fail closed for the implemented paths. The Action API success and structured-error
schemas now share a feature-gated offline fuzz boundary with the production semantic
validators. Pages, revisions, category members, image references, and image-info
collections have explicit deserialization cardinality ceilings.

**Residual risk and action.** The configured origin, direct-IP validation, bounded DNS
answer, new-connection DNS revalidation, proxy exclusion, and every redirect now fail
closed under one source policy. Scripted resolver tests cover mixed answers and a
public-to-loopback rebinding sequence; loopback fixtures cover the real request path
and same/cross-origin redirects. `reqwest` may reuse an already-vetted pooled socket
without asking DNS again, which cannot retarget that established socket but should be
re-reviewed if connector or proxy behavior changes. Add explicit compressed-byte and
decompression-ratio tests if content encoding is enabled. Add total operation,
parser-output, allocation, and disk-growth budgets independent of server metadata.
Continue longer native fuzz campaigns across JSON/schema adapters, continuation
handling, the wikitext transformer, pack decoding, and delta reconstruction. Exercise
slow-loris, truncation, redirect, and retry storms in offline fixtures.

### Hostile HTML, links, and media

**Scenario.** Wikitext attempts script execution, attribute injection, unsafe URL
schemes, remote tracking loads, CSS abuse, active SVG, decompression bombs, huge image
dimensions, or misleading link destinations.

**Present resistance.** Raw tags do not pass through as active article HTML; generated
values are escaped or emitted through the Markdown event renderer; URL schemes are
restricted; CSP blocks scripts, network connections, remote styles/fonts, and
framing; reader assets are bundled. Optional media is default-off and accepts only
bounded, completely decoded passive JPEG/PNG bytes with MIME/magic agreement,
dimension/pixel/allocation ceilings and APNG rejection. Wikimedia CDN downloads use
one explicit source-derived HTTPS origin with the same DNS/rebinding controls as API
requests. Reader/export access re-verifies the content hash and raster before serving
or writing a fixed MIME type with `nosniff`; remote URLs are user-clicked attribution
links, never resource loads. Media inventory and placements are included in new
predecessor-linked manifests.

**Residual risk and action.** Continue malicious image corpus/property testing and
measure decoder CPU under maximum permitted inputs. External links are an intentional
user-initiated escape from offline mode and must be visually identifiable; consider
an interstitial or opt-in policy where privacy expectations require it. Third-party
cross-origin media CDNs remain fail-closed until they have an equally explicit
source-bound policy.

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
forbidden workspace `unsafe` reduce accidental and some supply-chain risk. Candidate
archive/checksum/signature tooling is bounded and tested; its CI dry run has no
publication or secret authority.

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
focused tamper tests catch many partial or accidental changes. An external Ed25519
anchor binds a public key to one exact sequence and manifest identity. CLI inspection
distinguishes an invalid signature, a different (including stale) head, other local
verification failure, and an authenticated current head. CLI rotation requires an
authenticated current anchor, creates a new key and recovery copy at create-new paths,
then atomically replaces the current anchor without deleting either key.

**Residual risk and action.** The anchor embeds its verification key, so its trust
comes from separately protected retention and an operator's recorded public-key,
sequence, and manifest identity—not from a global PKI. Replacing both the library and
its only anchor defeats rollback detection. Keep the private key backup and anchor
history in separate failure domains, compare restores with the exact anchor retained
for that backup, and never reset trust merely because an anchor is missing or stale.
Key revocation and cross-device trust distribution remain policy work. Full
revision-chain policy completeness, manifest-to-database snapshot reachability, and
detection of database truncation beyond what an exact external-head mismatch exposes
remain limited.

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
Current and historical Markdown/plain-text exports are private derived directories
with per-article source, revision, author, capture, transformer, and content-hash
provenance. Historical slices use an inclusive revision-time cutoff and do not replace
the maintained current export.

**Residual risk and action.** Onboarding, export, backup, and restore documentation
must warn about retained suppressed material and legal obligations. Diagnostic
redaction must remain an explicit versioned allowlist, and users must review bundles
before sharing them. Define bounded log retention and extend the native release audit
to GUI launch. The current macOS/Linux workflow denies and records libc IPv4/IPv6
connection, addressed-datagram, and hostname-resolution attempts while exercising
offline CLI commands, idle daemon IPC, and a browser-like default-reader crawl.
Destructive purge is implemented
and validated through the CLI, daemon, and Iced GUI. The library
foundation implements bounded preview and authorization, authenticated purge events,
exact authorized-absence verification, and restartable loose, whole-pack, and retained-
subset mixed-pack cleanup. The writer boundary adds exact confirmations, one-lease
execution, and fail-closed daemon startup recovery. The GUI uses the same bounded
mutation and requires a local preview, exact typed name/fingerprint, and two initially
unchecked acknowledgements that reset when the preview changes. The erasure-limit
contract and non-promises remain defined in `docs/operations/destructive-purge.md`.

### Destructive purge authorization and recoverability

**Scenario.** A user or local client mistakes non-destructive collection removal for
data erasure; an unauthorized, stale, or partial purge removes shared or manifested
canonical content; a crash strands retained pack deltas; or the product implies that
unlinking local files removed sensitive material from backups or physical media.

**Required controls.** Purge remains a distinct tombstone-only operation. A complete
non-mutating preview must bind the exact collection name and generation, current
manifest head, complete logical/physical catalog fingerprint, exclusive/shared
closure, and estimated reclamation into a typed preview fingerprint. Commit requires
the exact name and fingerprint plus explicit acknowledgements that audit metadata and
hashes remain and that external copies are unaffected. The daemon or direct writer
revalidates every binding under the single-writer boundary; client UI state is not
authorization.

Before any payload becomes absent, a typed predecessor-linked purge event must
authenticate a durable journal inventory. Mixed packs are rewritten into fully
verified replacement generations for all retained entries and delta dependencies
before the old generation is retired. Shared or uncertain references are retained.
Journal phases are monotonic and idempotent, restart before conflicting work, and
allow verification to distinguish unexplained loss, authorized absence, and pending
cleanup. An older reader that cannot interpret the event fails visibly rather than
silently accepting a shortened archive.

**Residual risk and non-promises.** Audit metadata and hashes remain deliberately,
including collection, page/revision, author/comment, membership, run, manifest, and
journal evidence. Shared payload remains. Purge neither contacts the source nor
removes copies from backups, filesystem/VM snapshots, exports, synchronized storage,
logs, crash artifacts, or other systems. File unlinking and pack retirement are not
secure erase and cannot prove removal from SSD wear leveling, copy-on-write history,
journaled blocks, caches, swap, or forensic remnants. Full-disk encryption and
separate retention/media-destruction controls remain external responsibilities.

## Present versus pending control register

| Requirement | State at review | Evidence / remaining work |
| --- | --- | --- |
| No telemetry or external reader assets | **Present for the tested default macOS release paths; Ubuntu GUI evidence pending** | Bundled CSS, restrictive CSP, an in-process populated-library crawl, and a native release-mode interposer audit for offline CLI commands, idle daemon IPC, the default reader, and a bounded no-action Iced launch. The integrated macOS Aqua run covered six reader routes and the packaged GUI with zero outbound attempts. A headless Linux pass must report GUI evidence as unavailable; native Ubuntu graphical evidence and reassessment of any future updater/telemetry behavior remain required |
| Explicit source allowlist | **Present for configured source and bounded Wikimedia CDN origins** | Each client derives exact normalized origins from its selected endpoint, plus only `https://upload.wikimedia.org:443` for recognized standard-port Wikimedia project sources; third-party/nonstandard/loopback sources remain exact-origin. Ambient proxies are disabled, unsafe literal and whole DNS answers are rejected, new connections revalidate DNS, and redirects must remain on an approved exact origin. Loopback HTTP enables a loopback-only fixture exception |
| HTTPS through Rustls | **Present** | `wikisync-mediawiki` disables reqwest defaults and enables `rustls-tls`; loopback HTTP is fixture-only by validation |
| Application User-Agent and operator contact | **Present with a bounded environment contract** | All production MediaWiki and dump clients use one `WikiSyncer/<version> (<contact>)` policy. `WIKISYNC_OPERATOR_CONTACT` accepts a bounded visible-ASCII public contact, rejects malformed values before contact without echoing them, and defaults to the public repository URL. Service-manager configuration is documented; there is no separate GUI settings control |
| Response, timeout, parser, redirect, and decompression bounds | **Partial** | Request/connect timeout, per-response and aggregate run-byte limits, clone-shared concurrency and byte-rate shaping, redirect count/origin policy, object/pack and inline-depth limits exist. Maintained bounded fuzz targets now cover Action API success/error schemas and continuations, content rewriting, trusted-head JSON, dump parsing, loose decompression, pack/index reads, repacking, and delta reconstruction. Action API collection cardinalities fail during typed deserialization; broader adversarial corpora, sustained native runs, decompression-ratio coverage, and non-JSON parser/allocation budgets remain |
| Sanitized HTML and restrictive CSP | **Present for the tested reader path** | Raw HTML events become text, links are rewritten, passive local media is revalidated before serving, and headers are tested; continue adversarial corpus testing |
| User-restricted data-directory permissions | **Present on Unix core paths** | Directories including manifests/exports are `0700`; SQLite/WAL/SHM and created manifest/export files are `0600`; extend release auditing to logs, IPC, and backups |
| BLAKE3 identities for canonical text/media | **Present** | Domain-separated text/media object kinds, bounded validated ingestion, and verified reader/export reads are implemented |
| Predecessor-linked manifests | **Present, optionally head-authenticated** | Bounded canonical JSON, BLAKE3 body identity, immutable run configuration snapshots, strict predecessor/sequence checks, atomic durable append, bounded crash-gap repair, catalog-difference revisions, resulting heads, deterministic media inventory/placements, and tamper tests are implemented. Schema-v1 manifests remain readable and explicitly report no media coverage. A signature covers a validated chain head rather than modifying every manifest file |
| Optional Ed25519 signatures and external trust anchor | **Present for explicit external Unix paths** | CLI generation, validation, create-new import, verified anchor export/explicit refresh, comparison, and recovery-preserving rotation are tested. The GUI generates/validates keys, verifies against an anchor, and retains a changed previous anchor during refresh. Keys and anchors must use explicit paths outside the library in private operator-owned storage; revocation and broader trust distribution remain policy work |
| Full verify contract | **Partial** | Loose/pack/delta objects, manifest identity/chain/run coverage, revision/page/object/media reachability, authenticated v2 media inventory/placements, page heads, checkpoint run/scope/boundary, search/FTS pointers, and search transformer versions are checked with bounded stable scans. An exact external head comparison detects a different restored head. The current schema has no derived-cache table; contentless FTS bodies, full revision-chain policy completeness, older v1 media coverage, and broader database truncation detection remain limited |
| Loopback-only read-only reader | **Present, confidentiality gap** | Non-loopback addresses are rejected and only GET routes exist; localhost is unauthenticated and not an OS-user boundary |
| Daemon single-writer authorization | **Present within the local-account trust boundary** | Versioned bounded Unix IPC, `0600` sockets inside the private library, cooperative writer leases, scheduling, signal cancellation, advisory-lock stale-socket recovery, GUI/CLI forwarding, and exclusion/recovery tests exist. On macOS/Linux, kernel-reported peer effective UIDs are checked before request processing and mismatches are dropped. Hostile same-UID processes remain explicitly trusted and can invoke permitted daemon operations; this control does not resist malware or account compromise |
| Destructive purge | **Present for the payload-only product workflow** | Bounded preview/authorization, typed purge events, authorized-absence verification, and restartable loose, whole-pack, and retained-subset mixed-pack cleanup are implemented. CLI/daemon add exact confirmations, one-lease execution, bounded IPC, and fail-closed startup recovery. Iced uses the same mutation with read-only preview, exact typed values, two reset-on-change acknowledgements, durable terminal receipts, and direct/daemon parity tests. `collection remove` remains non-destructive. Audit metadata, shared payload, external copies, backups, snapshots, exports, and storage-device remnants are explicitly not erased |
| Locked/audited dependencies | **Partial** | Lockfile, cargo-deny policy, locked CI tests, and immutable commit-SHA pins for all third-party normal/release workflow actions are present; formal audit response, SBOM, and provenance remain |
| Signed beta packages | **Credential-free substrate present; credentialed release pending** | Deterministic bounded macOS/Linux candidate archives, exact signed-set verification, detached OpenSSH Ed25519 checksum-signing hooks, a defined Linux archive/repository/key-distribution trust model, deterministic macOS Mach-O/signing plans, fail-closed Developer ID/receipt/final-archive validation, packaging-policy tests, a native zero-outbound default-path audit, and a no-secret/no-publish native CI dry run are present. Real Apple Developer ID signing, timestamping, notarization and clean-host Gatekeeper evidence; protected release identities and independent trust-anchor distribution; credentialed native validation; and signed publication remain |
| Safe optional media | **Present, default-off** | Bounded discovery/download, source-bound CDN policy, passive-raster validation, hard collection budgets, immutable attribution/licensing metadata, media-aware manifests/full verification, local-only reader serving, and attributed deterministic exports are fixture-tested. Higher-resolution/active media remain unsupported |
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
   event rewriting, Action API success/error schemas and continuations, trusted-head
   JSON, loose decompression, pack/index decoding, and delta reconstruction under
   explicit CPU/memory/output/depth/ratio limits.
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
14. **Media decision:** optional thumbnails remain default-off; their bounded passive-
    raster decode, MIME handling, CSP, source/license/attribution, budget, and malicious
    corpus tests all pass before beta. Text synchronization remains usable when an
    optional image fails. Active and higher-resolution media remain unavailable.
15. **Destructive purge:** retain passing coverage for complete preview binding, exact
    confirmations, tombstone-only enforcement, shared-reference closure, authenticated
    purge events, restartable cleanup, replacement-pack activation, authorized-absence
    verification, direct/daemon/GUI parity, and explicit non-erasure language. A
    partial or unvalidated surface is not completion.

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
