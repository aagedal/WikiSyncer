# Backup, restore, and migration

WikiSyncer's stable-v1 backup contract is a quiescent, permission-preserving,
whole-directory copy of the library. It is a directory contract rather than a custom
backup-file format. This preserves the SQLite database, its WAL state, immutable
loose objects, packfiles and indexes, manifests, and any other durable library-local
files together.

Never treat a copy of `library.sqlite3` alone as a complete backup. Canonical revision
bytes live below `objects/`, and a live SQLite database may also have `-wal` and
`-shm` files. Derived cache and exports are not substitutes for canonical objects.

## External signing key and trusted head

An optional Ed25519 trusted-head anchor authenticates one exact, fully verified
manifest-chain head. The private PKCS#8 signing key and canonical JSON anchor are
deliberately not stored in the library. Every trust command requires an initialized
library and explicit absolute paths outside its tree. Each path's parent must already
exist, be a non-symlink directory owned by the current user, be owner-writable and
searchable, and grant no group or other access (normally `0700`). Existing inputs and
refresh targets must be regular non-symlink files owned by the current user; new
targets must be absent. A signing-key input must be readable by its owner and have
mode `0600` or read-only `0400`; created files use `0600` on Unix.

Create, validate, or import a key as follows:

```sh
wikisync --library /absolute/path/to/library trust key-generate \
  --output /separate/private/location/wikisync-signing-key.pk8
wikisync --library /absolute/path/to/library trust key-validate \
  --key /separate/private/location/wikisync-signing-key.pk8
wikisync --library /absolute/path/to/library trust key-import \
  --source /separate/private/location/existing-key.pk8 \
  --output /separate/private/location/imported-key.pk8
```

Generation and import are create-new operations: they refuse to overwrite an entry.
Import validates and copies the key, requires different source and destination paths,
and retains the source. Keep a protected backup of the private key in a different
failure domain. The key is needed to refresh future anchors, but it is not needed to
inspect an existing anchor, which embeds its public verification key.

Create an anchor only after synchronization has reached the state to preserve:

```sh
wikisync --library /absolute/path/to/library trust anchor-export \
  --key /separate/private/location/wikisync-signing-key.pk8 \
  --anchor /separate/trusted/location/wikisync-trusted-head.json
wikisync --library /absolute/path/to/library trust anchor-inspect \
  --anchor /separate/trusted/location/wikisync-trusted-head.json
# Add --json to anchor-inspect for structured output.
```

`anchor-export` signs the observed head in memory, then requires a complete
authenticated full-library verification before publishing it. The default is
create-new. After a legitimate manifest advance, inspection reports
`different-head`; update the same file explicitly with `anchor-export ... --refresh`.
Refresh first requires the existing file to be a valid canonical anchor and installs
the replacement atomically, but the CLI does not retain the old anchor. Copy it to
independent history first when audit or recovery continuity matters. The GUI's
**Full verify, sign, and refresh anchor** action differs here: when the anchor changes,
it creates a sequence/key-named `.previous` file beside it before replacement.
The GUI can generate or validate a key at an explicit external path, verify against
an anchor, and perform that retaining refresh. It does not copy an imported key or
provide the CLI's recovery-preserving rotation command.

Inspection performs bounded full verification and reports one of
`authenticated-current`, `invalid-signature`, `different-head`, or
`local-verification-failed`. Only `authenticated-current` means that the external
anchor's signature, the exact current local manifest head, and the checked local
state agree. A valid older anchor and a valid anchor for another library both produce
`different-head`; investigate the expected sequence and manifest identity instead of
silently refreshing. A completed comparison prints its result without making every
mismatch a command error; automation should use `--json` and require the
`authenticated-current` comparison explicitly.

Rotate the signing identity with three distinct external paths:

```sh
wikisync --library /absolute/path/to/library trust rotate \
  --anchor /separate/trusted/location/wikisync-trusted-head.json \
  --new-key /separate/private/location/wikisync-signing-key-2.pk8 \
  --recovery-anchor /separate/trusted/location/wikisync-trusted-head-before-rotation.json
# Add --json for structured output.
```

Rotation first requires the current anchor to authenticate the current fully verified
library. It then creates and durably writes a new key, copies the old canonical anchor
to the create-new recovery path, and atomically replaces the current anchor with one
signed by the new key. It deletes neither key. A failure names the last durable phase;
inspect the stated files before retrying, and never overwrite the recovery anchor.

If a key is lost, retain its anchors: they can still verify states already signed,
although they cannot sign a later head. If an anchor is lost or its expected identity
is uncertain, establish the library state from an independently retained anchor,
known backup, recorded public-key/manifest identity, or other trusted evidence before
creating a replacement. A new self-signed anchor cannot make an uncertain library
trustworthy. The anchor authenticates captured bytes and chain continuity since
capture; it does not prove that upstream statements were true, complete, lawful, or
still available. Replacing both the library and its only external anchor defeats
rollback comparison.

## Create a backup

1. Record the WikiSyncer binary version and the library path.
2. Stop the daemon cooperatively and stop any GUI, CLI writer, or reader using the
   library. `systemctl --user stop wikisyncd.service` invokes cooperative shutdown.
   On macOS, or for a manually started daemon, call shutdown directly before unloading
   the launchd agent:

   ```sh
   /absolute/path/to/wikisyncd --library /absolute/path/to/library shutdown
   # For the installed macOS user agent, unload it after shutdown succeeds:
   launchctl bootout "gui/$(id -u)/org.wikisync.WikiSyncer"
   ```

3. Confirm the daemon is stopped. If the control socket remains after an unclean
   crash, the next start intentionally fails closed. Do not delete the socket until
   service-manager state and process inspection establish that no process owns the
   library. A stopped daemon's `status` command normally returns a connection error.
4. If external rollback detection is required, create a uniquely named anchor for
   this stopped state with `trust anchor-export` (without `--refresh`) and retain it
   separately from both the library and its backup. Record its printed sequence,
   manifest identity, and public key. Do not place the private signing key or the only
   anchor below the library root.
5. Copy the library root to a new, user-only destination on a filesystem that
   preserves permissions. For example:

   ```sh
   umask 077
   mkdir -p /absolute/path/to/backup-parent
   cp -a /absolute/path/to/library \
     /absolute/path/to/backup-parent/library-YYYYMMDD-HHMMSS
   ```

6. Compare the copied file count and allocated size with the source. Open the copy
   with the same WikiSyncer version, inspect `wikisync ... status`, and run the GUI's
   full-library verification. Full verification reconstructs and hashes logical
   canonical objects; a quick SQLite check alone does not.
   When step 4 created an anchor, also run `trust anchor-inspect` against the copied
   library and that exact separately retained anchor; require
   `authenticated-current` before accepting the backup.
7. Restart the original service only after the copy has finished.

Backups can contain material later deleted or suppressed upstream, public editor
identifiers, and exact article history. Store them with user-only permissions and
full-disk encryption where available. A separately encrypted, offline copy protects
against disk loss; ordinary cloud sync is not a safe live-library location.

## Restore without overwriting the existing library

Restore into a new empty destination so the existing library remains recoverable:

1. Stop all WikiSyncer processes for both source and destination paths.
2. Copy the backup directory to a new local path using a permission-preserving tool.
3. Set directory access to the owning user. WikiSyncer re-applies `0700` to library
   directories and `0600` to SQLite files it opens, but operators must also secure
   parent directories and any other copied files.
4. Reject a restore whose `library.sqlite3` is a symbolic link. Do not combine a
   database from one backup with `objects/` from another.
5. Open the restored path with the same binary version first, inspect status, and run
   full object and manifest-chain verification. If a trusted head was retained for
   this backup, compare the restored path with that exact external anchor:

   ```sh
   wikisync --library /absolute/path/to/restored-library trust anchor-inspect \
     --anchor /separate/trusted/location/library-backup-anchor.json
   ```

   Require `authenticated-current`. `different-head` is not proof of corruption by
   itself—it can mean the anchor is older, newer, or belongs to another library—but
   it means this anchor does not authenticate this restore and must be investigated.
6. Render a new service definition with the restored absolute path. Stop and remove
   the old service definition before enabling the new one.

A restored library whose full check passes demonstrates internal object and manifest-
chain consistency since capture. Without an externally retained signed manifest head,
it cannot detect replacement by an older internally consistent backup or prove
completeness relative to Wikimedia. The producing binary defines the durable entries
that belong to its stable-v1 whole-directory backup; no database-only or hand-selected
subset is a conforming backup.

## Upgrade and schema migration

`Library::open` applies forward SQLite migrations automatically. Stable v1 guarantees
preservation through supported monotonic migrations, not a frozen public table layout
or schema number. Merely running an offline CLI command such as `status` opens the
library and can migrate it.

For an upgrade:

1. Stop the daemon and make a tested whole-library backup with the old binary.
2. Keep the old binary and backup together. Do not test a new binary against the only
   copy.
3. Restore the backup to a staging path and open that copy with the new binary.
4. Inspect status, exercise representative search/show/history operations, and run
   full verification before upgrading the production path.
5. Stop the old service, update its executable path only after the checks pass, and
   start it once. Do not run old and new daemons concurrently.

Downgrades are not guaranteed. If rollback is required, stop the new binary and
restore the pre-upgrade directory as a unit; do not attempt to edit `PRAGMA
user_version` or reverse migrations manually.

## Move to another machine or filesystem

Use the restore procedure rather than moving a live directory. Preserve ownership and
permissions, install a compatible executable for the destination platform, and
re-render service templates with destination-specific absolute paths. Unix sockets
are runtime files and must not be used as evidence that a copied daemon is alive.
Start only one destination after verification to avoid two independent writers being
mistaken for one synchronized service.

## Current and historical exports

Exports are offline, derived Markdown or plain-text views with per-article source,
revision, capture, author, transformer, and content-hash metadata plus `index.jsonl`
and `manifest.json`. The current export is maintained at `exports/current`:

```sh
wikisync --library /absolute/path/to/library export --format markdown
wikisync --library /absolute/path/to/library export --format text --collection 7
```

Select an inclusive historical cutoff with either a positive captured revision ID or
an RFC 3339 timestamp ending in `Z` or a numeric UTC offset:

```sh
wikisync --library /absolute/path/to/library export --format markdown \
  --at 123456789
wikisync --library /absolute/path/to/library export --format text \
  --collection 7 --at 2026-08-19T12:15:00Z
```

For every selected page, historical export chooses the newest captured revision at
or before the cutoff. Pages with no captured revision by then are omitted and counted
as `uncaptured_page_count`. A revision selector uses that revision's timestamp for the
whole slice; it must occur among the selected pages and must be unambiguous across the
selected wikis. A single-collection selection resolves cross-wiki revision-ID
ambiguity.

Historical output uses a deterministic, selector/format/scope-specific directory
below `exports/` and schema `wikisync-historical-export-v2`; it does not replace
`exports/current`, whose schema is `wikisync-current-export-v2`. The v2 schemas add
an optional hash-addressed `media/` directory, per-article media metadata and
attribution sections, and manifest media counts; their manifests identify the v1
predecessor explicitly. Repeating the same
selection atomically replaces that same export directory after staging succeeds.
Exports are bounded and private on Unix, but they can contain authors and material
later removed upstream. They are not canonical evidence, a complete-library backup,
or proof that the transformed content is true.
