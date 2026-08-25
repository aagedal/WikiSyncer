# Destructive purge contract

Destructive purge is not implemented in the current WikiSyncer CLI, daemon, or GUI.
`collection remove` remains a non-destructive operation: it tombstones a collection,
stops future tracking, and retains every captured payload and audit record. Do not
delete SQLite rows, loose objects, packs, indexes, or manifests by hand to simulate a
purge. Manual deletion can strand pack-delta dependants and can make manifested
content disappear without an authenticated explanation.

This document defines the required contract for the complete purge product. A partial
library foundation now implements bounded preview/authorization, the authenticated
manifest event, exact authorized-absence verification, and restartable cleanup for
loose objects and whole-target packs. Mixed-pack retained-subset replacement,
single-writer startup/resume integration, and the CLI/daemon/GUI workflow remain
unfinished. The complete implementation must satisfy this contract and its
failure-path tests before a product surface may describe purge as available.

## Meaning and scope

Purge is a separately authorized attempt to reclaim canonical text and media payload
that is exclusive to one tombstoned collection. It is not another collection-removal
mode. The selected collection remains tombstoned, and WikiSyncer retains the identity
and audit information needed to explain what was captured, what was authorized for
removal, and why a canonical payload may now be absent.

At minimum, retained audit state includes the collection identity, exact collection
name, generation and tombstone state; source, page, revision, run, checkpoint, and
capture identities and timestamps; configuration and membership history; manifests;
content-object identities, kinds, and recorded lengths; the purge authorization
event; and the cleanup journal and its inventory hashes. Author and edit metadata
that was already retained is not promised to be removed. A later schema may retain
additional bounded evidence when it is necessary to verify chain continuity or
explain shared references.

A payload is collection-exclusive only when a bounded, complete reference-closure
scan proves that no retained payload root outside the selected collection still needs
it. Those roots include every other active or tombstoned collection; source-wide or
other-scope revision/media claims, page heads, checkpoints, and resumable jobs; purge
records other than this exact authorization; and any other current-schema reference
whose payload-retention meaning is not superseded by this exact purge event. The
selected collection's old manifests and metadata remain audit evidence, but only the
new purge event can authorize their exact payload references to become absent.
Content deduplicated under one logical object ID is shared even when the human-
readable pages differ. Uncertainty, an unsupported metadata kind, an incomplete scan,
or concurrent catalog change must retain the object and fail or narrow the preview;
it must never make deletion more permissive.

Physical pack dependencies are a second closure. A retained delta entry must not lose
its base, even if the base's logical payload would otherwise be purgeable. Objects in
a mixed pack are not reclaimed by deleting the pack. The implementation must first
reconstruct retained entries into a verified replacement generation as described
below.

Logical payload bytes and physically reclaimable bytes are different quantities.
Deduplication, shared references, mixed packs, filesystem allocation, and retained
audit records can make the eventual space reduction smaller than the exclusive
logical payload. A preview and completion receipt must report those quantities
separately and must describe physical reclamation as an estimate until cleanup has
completed.

## Complete preview and explicit authorization

Preview is mandatory and non-mutating. It must be produced through bounded,
read-only library APIs and must either cover the complete relevant catalog or fail
closed. A truncated list may be displayed for human readability only when its totals
and fingerprint still cover the complete inventory; a partial scan is never an
authorization preview.

The preview must identify:

- the collection ID, exact current name, tombstoned status, tombstone time, and
  monotonic collection generation;
- the exact current manifest-chain head sequence and identity, or an explicit
  absence when the lower manifest contract permits an empty chain;
- a domain-separated purge-preview fingerprint committing to the contract version,
  collection identity and name, tombstone/generation state, manifest head, catalog
  fingerprint, complete exclusive/shared inventory identities, and reported totals;
- exclusive object counts and canonical bytes, by text and media kind, while clearly
  stating that shared and uncertain objects are excluded from the target;
- verified loose-object counts, affected/whole/mixed pack counts, currently
  reclaimable bytes, and a warning that mixed-pack reclamation requires replacement
  work and temporary coexistence with the old generation;
- the audit metadata and hashes that will remain, including whether an earlier purge
  journal is resumable; and
- the recoverability and erasure limitations in this document.

Commit must be a separate explicit action. It must require all of the following,
without defaults, an ambient interactive `yes`, or a generic force flag:

1. the exact current collection name typed or supplied again;
2. the complete purge-preview fingerprint typed or supplied again; and
3. an explicit acknowledgement that purge targets only local canonical payload and
   deliberately retains audit metadata and hashes; and
4. an explicit acknowledgement that WikiSyncer does not remove or prove the absence
   of copies in backups, filesystem or virtual-machine snapshots, exports,
   synchronized folders, SSD remnants, or other external storage.

The acknowledgement records informed authorization; it is not evidence that a
backup exists or that any external copy was deleted. A GUI must leave it unchecked
and clear the typed confirmations whenever the preview changes. CLI and GUI may help
copy identities, but they must not silently populate or accept the confirmations.

The writer must rederive and compare the tombstoned status, collection generation,
manifest head, complete catalog and inventory, and preview fingerprint
while holding the same direct-writer lease or daemon-owned single-writer boundary
used by other mutations. Any mismatch rejects the commit before destructive progress
and requires a new preview. The daemon must enforce the same checks; a client-side
button or argument parser is not an authorization boundary. An active or missing
collection is always rejected, including on a resumed or directly invoked operation.

Purge is local and requires no MediaWiki request. A compatible daemon may receive a
small, bounded authorization record, but not an unbounded client-supplied deletion
list. The durable library derives the authoritative closure.

## Manifest and cleanup ordering

An authorized absence must never be indistinguishable from tampering or accidental
loss. Manifest schema 3 therefore adds a backward-readable, explicitly typed purge
event while new readers continue to read older synchronization manifests. An older
reader that does not understand the purge event must fail visibly as incompatible; it
must not skip the event and report the shortened library as clean.

The purge event must commit to the selected tombstone, the pre-purge manifest head,
the collection generation, the complete catalog and preview fingerprints, the
identity of the durable purge inventory, its counts and byte totals, the retained
audit boundary, and the authorization time. It must not contain raw confirmation
inputs beyond the already retained exact collection name. The event becomes the new
manifest-chain head and causes a separately retained older trusted head to compare as
different, as any legitimate manifest advance does.

The implementation must use a durable, monotonic, idempotent cleanup journal. Exact
table and file layouts are private, but the observable ordering is:

1. Revalidate the complete preview under exclusive writer ownership and durably
   install a prepared journal containing the bounded inventory and its identity.
   No canonical payload is absent at this phase.
2. Append and durably install the purge manifest event that authenticates that
   inventory. No payload may be made logically or physically absent before this
   succeeds.
3. For every pack containing both purgeable and retained entries, reconstruct all
   retained entries, including every delta dependency, into a new pack generation.
   Verify the complete replacement pack and index and atomically activate their
   locations. Keep the old pack and index intact through this phase.
4. In a bounded transaction, mark the selected exclusive payload and its references
   with an explicit authorized-absence state and remove only derived indexes that
   would still expose that payload. Preserve the audit metadata, object identities,
   reference identities, inventory identity, manifest event, and journal needed by
   verification. No retained logical object may depend only on a location scheduled
   for retirement.
5. Retire target loose bytes and old pack/index generations only after all retained
   replacements are active and verified. Missing already-retired targets are
   idempotent only when the journal and purge event authorize that exact absence.
   Unexpected files, identities, paths, or generations fail closed.
6. Sync affected directories, record actual reclaimed storage, and durably mark the
   journal complete. Only then may the product report purge complete.

If the implementation needs a more conservative order for a storage backend, it may
retain extra bytes longer, but it must not weaken these prerequisites. In particular,
disk exhaustion during replacement-pack construction leaves the old pack active and
readable. A pack is never deleted merely because it contains a purge target.

Cancellation and process termination are observed only at bounded safe boundaries.
After restart, the single writer must discover and resume or safely finalize an
unfinished purge before permitting a conflicting mutation. Every phase is
repeatable: a crash before the manifest event leaves all payload present; a crash
after the event is explained by the journal; a crash after replacement activation
retains the old generation until cleanup can prove it is safe to retire; and a crash
after unlinking bytes resumes directory synchronization and final accounting. A
retry never creates a second authorization event for the same journal identity.

Full verification must distinguish at least:

- unexplained missing manifested payload, which remains a failure;
- an exact absence authorized by a valid purge event and inventory;
- a purge that is valid but still has pending cleanup; and
- journal, inventory, pack-replacement, or catalog state that disagrees with the
  authenticated event, which is a failure.

Verification must continue to reconstruct and hash every retained object and validate
all shared references. After completion and full verification, an operator may
choose to refresh an external trusted head. The prior trusted head and anchor history
should be retained separately because they authenticate the pre-purge state and the
fact that the chain advanced; replacing the only anchor discards that comparison.

## Explicit non-promises

Purge does not promise complete personal-metadata erasure. Retained collection,
source, page, revision, author, comment, membership, run, manifest, journal, object-ID,
and timestamp evidence can itself contain or reveal personal or sensitive
information. Shared canonical payload also remains whenever another retained root
needs the same logical object.

Purge does not contact or alter the upstream wiki and cannot remove content from
MediaWiki, mirrors, search engines, or other users' copies. It does not delete or
rewrite whole-library backups, Time Machine or filesystem snapshots, virtual-machine
images, cloud or synchronized copies, exported Markdown/plain text/media,
diagnostic bundles, logs, crash dumps, swap, or temporary copies outside its exact
managed inventory. Operators must handle those copies under their own retention and
legal policies.

Purge is not secure erase. WikiSyncer may unlink managed files and retire pack
generations after durable verification, but it does not overwrite physical sectors,
issue device sanitize commands, destroy encryption keys, or prove that bytes are
unrecoverable from SSD wear leveling, filesystem copy-on-write history, journaled
blocks, storage-controller caches, forensic remnants, or deallocated space. Use
full-disk encryption and platform-appropriate media-destruction or cryptographic-
erasure procedures when those guarantees are required.

Purge also does not guarantee that allocated filesystem usage falls by the logical
payload size. It may temporarily increase usage while replacement packs coexist with
old generations, and durable audit metadata remains permanently by design.
