# Operations guide

This guide describes the behavior present in the repository, not a signed beta
installer. WikiSyncer currently provides source builds and parameterized user-service
templates. It does not yet provide signed macOS or Linux packages or a stable backup
format. The CLI provides an offline redacted `doctor` report/bundle, and the GUI
includes the current schedule editor.

- [Service management](service-management.md): install, start, stop, uninstall,
  permission, and sleep/wake guidance.
- [Backup, restore, and migration](backup-restore-migration.md): take a quiescent
  whole-library copy and test upgrades without risking the only copy.
- [Diagnostics](diagnostics.md): collect local, redacted evidence and distinguish
  database checks from canonical-object verification.

A library may retain public editor names or IP addresses and material later deleted
or suppressed upstream. Backups and diagnostic output must be protected and reviewed
before sharing. A successful integrity check means the captured bytes still match
their recorded identities; it does not establish that the content is true.
