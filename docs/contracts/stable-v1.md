# Stable v1 compatibility contracts

This document defines the compatibility boundary for WikiSyncer's stable-v1 command
line, durable library, exports, and backups. Canonical revision bytes and their
logical content identities remain authoritative; configuration, indexes, rendered
exports, and physical pack locations must never become a second source of truth.

## CLI JSON v1

Every successful general CLI `--json` invocation listed below emits one UTF-8 JSON
object followed by a newline. The top-level object has `"schema_version": 1`.
Consumers must check that value before interpreting command-specific fields. JSON
object-key order and pretty-print whitespace are deterministic for reproducible
fixtures, but consumers must not depend on them. `doctor` and export files use the
independent format identifiers described below.

The v1 command-specific top-level fields are:

| Command | Required fields in addition to `schema_version` |
| --- | --- |
| `source add` | `wiki_id`, `api_endpoint`, `language_code`, `created` |
| `source remove` | `wiki_id`, `removed` |
| `source list` | `sources` |
| `collection add`, `edit`, `estimate` | `operation`, `committed`, `configuration`, `preview`, `result` |
| `collection remove` | `operation`, `committed`, `collection`, `result`, `effect` |
| `collection list` | `includes_tombstones`, `collections` |
| `category-preview` | `root`, `recursion_depth`, counts, `limits`, `categories`, `pages` |
| `search` | `results` |
| `show` | wiki/page/revision identity, `format`, `content` |
| `history` | wiki/page identity, `current_revision_id`, `revisions` |
| `diff` | wiki/page/revision identity, `mode`, `has_changes`, `lines` |
| `status` | `state`, `checkpoints`, `runs` |
| `trust anchor-inspect` | `comparison`, `anchor`, `verification` |
| `trust rotate` | `previous`, `current`, `recovery_anchor_created` |

IDs and counts are JSON integers. Optional values are represented by `null`, not by
sentinel strings or negative integers. Lists are always arrays, including when empty.
The `search` result is an object containing `results`; the pre-v1 bare array is not a
stable contract. Enum spellings such as `idle`, `reading`, and
`authenticated-current` are part of v1.

Within schema version 1, required fields are not renamed or removed, their JSON types
and meanings do not change, and enum values are not repurposed. New optional fields
or enum values may be added, so consumers should ignore unknown object fields and
handle unknown enum values explicitly. A change that invalidates those rules requires
a new `schema_version`; a future binary may offer old and new versions during a
documented transition.

`doctor --json` is independently identified by
`format: {"name":"wikisync-doctor","version":1}` because the same document can be
written as a diagnostic bundle. Export files have the schemas below. Human-readable
stdout, stderr wording, and JSON key order are not API contracts. Command failures
return nonzero and write a human-readable diagnostic to stderr; v1 does not define a
machine-readable error envelope. Security-sensitive commands such as `doctor` and
trust-key operations define and test their own redaction guarantees, but callers must
not treat arbitrary CLI error text as secret-safe structured output.

Offline golden fixtures under `fixtures/contracts/cli/` pin representative byte
serialization. Other CLI integration tests check the version and semantic fields for
source administration, collection administration, preview, search, show, history,
diff, status, and doctor output.

## Configuration and database v1

The library root is selected by `--library <path>` or, when that option is absent,
`WIKISYNC_LIBRARY`. An explicit option takes precedence. Stable v1 has no editable
configuration file: sources, collections, schedules, budgets, removal policy, network
transfer policy, checkpoints, and synchronization state live transactionally in
`library.sqlite3`. Supported CLI/GUI operations are the configuration API. Direct SQL
edits, copying individual tables, changing `PRAGMA user_version`, and treating a table
layout as a public API are unsupported.

The database compatibility promise is forward migration, not a frozen SQL schema:

- a stable-v1-or-newer binary may add transactional, monotonic migrations;
- a migration must preserve canonical object identity, captured revision/page/source
  identity, collection membership/history, synchronization durability, manifests,
  and user policy unless a separately documented user action changes them;
- failure must leave the pre-migration library recoverable from the required backup;
- opening with an older binary after migration is not supported; restore the complete
  pre-upgrade backup instead of attempting a schema downgrade; and
- `library.sqlite3` is not a complete library because canonical data also lives in
  content-addressed files.

Routine read-only inspection should use read-only library APIs. Commands that open a
writable `Library` may apply pending migrations, so operators must make a verified
backup before upgrading the only copy.

## Export v2

`exports/current/manifest.json` identifies schema
`wikisync-current-export-v2`. Historical output identifies
`wikisync-historical-export-v2`. These schemas supersede the corresponding v1
schemas by adding attributed, locally stored media; each v2 manifest records its v1
predecessor and the additive evolution. The manifest schema governs `manifest.json`, every
line of `index.jsonl`, article filenames, and article metadata. Each export contains:

```text
articles/<page-id>-<safe-slug>.<md|txt>
media/<content-object-id>.<jpg|png>  # present only when selected revisions have media
index.jsonl
manifest.json
```

The manifest records format, scope, counts, canonical source bytes, hash algorithm,
transformer version, maximum capture time, and media object/placement counts and
bytes. A historical manifest also records its inclusive revision/time selector.
Index rows and article metadata preserve wiki, page, revision, capture, source
URL/API endpoint, content hash, author when available, transformer provenance, and a
bounded media array. Each media entry records the hash-addressed relative path,
placement, caption/alternative text, source file identity and SHA-1, observed
rendition URL, description URL, author/attribution, license, dimensions, MIME type,
capture time, and local content hash. Markdown and plain text include equivalent
source, attribution, license, and provenance sections.

For unchanged canonical input, scope, selector, format, and transformer version, the
export is byte deterministic. Installation uses a private staging directory and
atomic directory replacement; a failed rebuild retains the previous complete output.
Historical exports never replace `exports/current`. Symlinked output components are
rejected. Identical media objects are written once per export even when several
placements reference them. Export reads verify the object identity and repeat the
bounded complete passive-raster validation before writing JPEG or PNG bytes.

Exports are derived interchange views. They can be rebuilt offline from canonical
objects, are not integrity evidence, and are not backups. A transformer change may
change derived bytes and must change its transformer version. An incompatible layout
or metadata change requires a new export schema.

## Quiescent whole-library backup v1

The stable v1 backup representation is a permission-preserving, quiescent copy of the
entire library directory. It is deliberately a directory contract, not a custom
archive container. There is no valid database-only backup.

Before copying, cooperatively stop the daemon and all GUI/CLI readers and writers. A
complete copy preserves every regular directory entry as a unit, including at least:

- `library.sqlite3` and any SQLite `library.sqlite3-wal`/`library.sqlite3-shm` files
  that still exist;
- `objects/`, including loose objects and immutable pack/index generations;
- `manifests/`; and
- any other library-local durable files introduced by the producing binary.

`exports/` and disposable `tmp/` contents are not canonical, but a whole-directory
copy may retain them. Do not combine a database, objects, packs, or manifests from
different snapshots. Active Unix sockets are not backup data; their presence means
the quiescence check is incomplete and the copy must not start.

Signing keys and trusted-head anchors use explicit paths outside the library and are
not silently included. Back them up in separate protected failure domains. When
rollback detection matters, retain the exact trusted head for the stopped snapshot
separately and require `trust anchor-inspect --json` to report
`authenticated-current` on the restored copy.

Restore to a new empty, private directory rather than overwriting the only working
library. First open it with the producing binary when available, run status and full
logical verification, compare the separately retained anchor when applicable, and
only then test migration with the newer binary. A clean internal verification proves
the checked captured bytes and links are self-consistent; it does not prove upstream
truth or detect rollback to an older self-consistent backup without an external
anchor.

The operational stop/copy/verify and restore sequences are expanded in
`docs/operations/backup-restore-migration.md`. A future portable archive format would
be an additional versioned format, not an implicit reinterpretation of this directory
contract.

## Pending destructive purge contract

Destructive purge is not part of the currently implemented stable-v1 CLI, daemon, or
GUI. `collection remove` has one stable meaning: stop tracking by tombstoning the
collection while retaining captured payload and audit history. A future purge must be
a separate preview-first operation and must not repurpose that command or its JSON
fields.

The normative product and durability requirements for that future operation are in
`docs/operations/destructive-purge.md`. In summary, purge may reclaim only canonical
text/media payload proved exclusive to one tombstoned collection. It retains audit
metadata and content hashes; requires exact collection-name, preview-fingerprint, and
explicit payload-only and external-copy acknowledgements; binds authorization to the
collection generation, manifest head, and complete catalog fingerprint;
authenticates absence with a typed manifest event and durable cleanup journal;
preserves shared objects; and activates verified replacement packs before retiring
old packed bytes.

This pending contract is not a claim of personal-metadata erasure, secure erase, or
removal from backups, snapshots, exports, synchronized copies, or SSD remnants. When
the command is implemented, its successful JSON shape and any durable manifest/journal
format must be independently versioned and added to this compatibility document
before being called stable.
