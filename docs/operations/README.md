# Operations guide

This guide describes the behavior present in the repository, not a signed beta
installer. WikiSyncer currently provides source builds, reproducible unsigned macOS
and Linux release-candidate archives, detached checksum-signing/verification hooks,
and parameterized user-service templates. It does not yet provide signed/notarized
macOS installers, a Linux repository/package trust chain, or a portable backup
archive. Stable v1 instead defines a permission-preserving, quiescent copy of the
complete library directory as its backup representation; a database-only copy is not
a backup. The CLI provides an offline redacted `doctor` report/bundle, and the GUI
includes the current schedule editor.

- [Service management](service-management.md): install, start, stop, uninstall,
  permission, and sleep/wake guidance.
- [Backup, restore, and migration](backup-restore-migration.md): take a quiescent
  whole-library copy, manage external signing keys and trusted-head anchors, compare
  a restore with its retained anchor, and test upgrades without risking the only
  copy.
- [Diagnostics](diagnostics.md): collect local, redacted evidence and distinguish
  database checks from canonical-object verification.
- [Destructive purge](destructive-purge.md): understand the preview-first payload-only
  workflow, retained audit evidence, shared-reference safety, restart recovery, and
  explicit non-erasure guarantees before using the CLI, daemon, or GUI operation.
- [Packaging](../../packaging/README.md): build and verify reproducible candidate
  archives, authenticate their checksum manifest, and understand the remaining
  credentialed platform-release gates.
- [Release acceptance matrix](release-acceptance-matrix.md): record per-candidate
  macOS/Ubuntu validation without treating workflow definitions or credential-free
  dry runs as signed-release evidence.

A library may retain public editor names or IP addresses and material later deleted
or suppressed upstream. Backups and diagnostic output must be protected and reviewed
before sharing. A successful integrity check means the captured bytes still match
their recorded identities; it does not establish that the content is true.

`wikisync export` creates provenance-bearing Markdown or plain-text derived views.
Without `--at` it maintains `exports/current`; with a captured revision ID or RFC
3339 timestamp it creates a separate historical time slice and leaves the current
export untouched. Exports are useful interchange artifacts, not canonical backups.
