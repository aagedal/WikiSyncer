# Packaging and service templates

WikiSyncer has a credential-free, reproducible release-candidate archive pipeline for
macOS and Linux. It does not yet have signed/notarized installers. An archive becomes
a publisher-authenticated release only after a release operator signs `SHA256SUMS`
with a protected identity, verifies that detached signature, completes the applicable
platform-signing gates, and publishes the reviewed files together.

## Reproducible archives

Build the three executables for the target on that target's runner, then package them
with a fixed timestamp:

```sh
cargo build --workspace --bins --release --locked
SOURCE_DATE_EPOCH="$(git show -s --format=%ct HEAD)" \
  python3 packaging/scripts/release.py package \
    --input-dir target/release \
    --output-dir target/release-candidate \
    --version 0.1.0 \
    --target-os macos \
    --target-arch aarch64
python3 packaging/scripts/release.py checksums \
  --output-dir target/release-candidate
python3 packaging/scripts/release.py verify-checksums \
  --checksum-file target/release-candidate/SHA256SUMS
python3 packaging/scripts/release.py verify-archive \
  --archive target/release-candidate/wikisync-0.1.0-macos-aarch64.tar.gz
```

The archive builder accepts only regular, executable input files named `wikisync`,
`wikisyncd`, and `wikisync-gui`. It normalizes member order, timestamps, ownership,
and permissions; includes the license, operations guide, and target service template;
and rejects symlink inputs. Run it on the intended target: it does not cross-compile,
inspect the executable format, produce a macOS application bundle, or imply platform
code signing. `release.yml` builds and validates unsigned candidates on native macOS
and Linux runners but intentionally publishes nothing.

Run the packaging tests without a Rust build:

```sh
python3 -m unittest discover -s packaging/tests -v
```

## macOS signing and notarization preparation

On a native unsigned build, `macos-signing-plan` checks that the exact three inputs
are executable, structurally bounded Mach-O containers for the requested architecture
and emits a canonical JSON plan. It validates thin/fat headers, slice ranges, CPU types,
and load-command bounds; Apple's native signing checks remain the authority on whether
the binaries are signable and valid to execute. Pull-request CI uses only the explicitly
unprovisioned dry-run identity and all-zero certificate fingerprint. That plan exercises
no credential and proves no signature or notarization status.

A protected release run supplies the exact Developer ID Application authority, Team
ID, and nonzero certificate SHA-1 fingerprint. After the operator performs the
planned signing with hardened runtime and a secure Apple timestamp,
`verify-macos-signatures` fail-closed checks all three binaries with Apple's fixed
`/usr/bin/codesign`. `validate-notarization-receipt` then checks an authorized
notarytool JSON result against its separately recorded submission UUID and requires
status `Accepted`. After packaging, `verify-macos-release-archive` reruns the signature
policy and proves that the exact validated executable bytes are present in the macOS
archive.

The current `tar.gz` cannot carry a stapled notarization ticket. A ZIP, PKG, or DMG
must be used for Apple submission, and the published tar must receive clean-system
Gatekeeper validation before release. See
[macOS signing and notarization gate](../docs/security/macos-signing-notarization.md)
for the exact trust boundary and credentialed sequence.

## Detached checksum signatures

The signing hook uses the OpenSSH signature format with namespace
`wikisync-release`. Keep the private Ed25519 key outside the repository and staging
directory, pass it by path rather than environment value, and restrict it to mode
`0600` or stricter. The tool refuses symlink keys, keys owned by another user, and
overwriting an existing signature unless `--force` is explicit. It never copies the
private key into an archive.

```sh
python3 packaging/scripts/release.py sign-checksums \
  --checksum-file target/release-candidate/SHA256SUMS \
  --private-key /secure/offline/wikisync-release-ed25519
python3 packaging/scripts/release.py verify-signature \
  --checksum-file target/release-candidate/SHA256SUMS \
  --signature target/release-candidate/SHA256SUMS.sig \
  --allowed-signers /trusted/wikisync-allowed-signers \
  --signer-identity release@wikisync
```

The allowed-signers file is an independently obtained trust anchor in OpenSSH form,
for example `release@wikisync ssh-ed25519 AAAA...`. Do not distribute a newly created
allowed-signers file beside a release and treat that circular distribution as proof
of identity. For downloaded releases, prefer the single fail-closed command that
verifies the signature before interpreting the manifest and rejects unmanifested
archives:

```sh
python3 packaging/scripts/release.py verify-release \
  --checksum-file ./release/SHA256SUMS \
  --signature ./release/SHA256SUMS.sig \
  --allowed-signers /trusted/wikisync-allowed-signers \
  --signer-identity release@wikisync
```

The production release key and its independent distribution channels have not yet
been established. Test keys and CI dry runs prove only the mechanics. See
[Linux package and repository trust](../docs/security/linux-package-repository-trust.md)
for the complete upstream archive, public-key rotation, APT/RPM repository, and
third-party packaging boundaries.

`verify-release` copies each bounded archive from one securely opened descriptor into
an unlinked temporary snapshot while calculating its signed SHA-256. Archive layout is
then checked from that same snapshot, so replacing the download path between the digest
and layout phases cannot substitute different bytes. Verification therefore needs
temporary storage up to the size of the largest release archive being checked.

Platform release gates remain credentialed manual work:

- On macOS, sign all Mach-O executables with an explicit Developer ID Application
  identity, assess the signatures and hardened-runtime policy, notarize the final
  distribution artifact with protected App Store Connect credentials, staple where
  the format permits, and validate with Gatekeeper on a clean supported system.
- On Linux, provision and independently publish the project release public key before
  calling an archive a signed Linux package. The trust model is now defined, but no
  production identity or APT/RPM repository exists. The SSH checksum signature
  authenticates the reviewed upstream archive set; it is not an APT/RPM repository
  or native-package signature.

Never place signing keys, notarization credentials, or generated credential files in
the repository, ordinary CI logs, release archives, or diagnostic bundles.

## User-service templates

These templates run `wikisyncd` as the logged-in user. They do not install files,
create a library, configure synchronization schedules, or grant system-wide access.
The daemon remains in the foreground under the service manager and is the long-lived
writer for one library.

The daemon handles its IPC `shutdown` command, `SIGTERM`, and `SIGINT` through the
same cooperative stop path after the active operation completes. The systemd unit
uses `ExecStop` to request IPC shutdown; operators should still call `shutdown`
before a deliberate launchd `bootout` so failures are visible.

Replace every template token before installation:

- `@WIKISYNCD@`: absolute path to the `wikisyncd` executable;
- `@LIBRARY@`: absolute path to an existing WikiSyncer library;
- `@LOG_DIRECTORY@`: existing user-only log directory (launchd only); and
- `@LOG_MAINTENANCE_SCRIPT@`, `@NEWSYSLOG_CONFIG@`, and `@SERVICE_PLIST@`:
  absolute installed paths to the corresponding rendered launchd assets;
- `@UID@` and `@GID@`: the installing user's numeric IDs (launchd only); and
- `@DOCUMENTATION_DIRECTORY@`: absolute path to `docs/operations` (systemd only).

Do not put shell syntax, environment variables, `~`, XML entities, or systemd `%`
specifiers in a replacement. Service managers do not perform ordinary shell
expansion here. Paths containing `@`, a newline, or a double quote are not supported
by this simple substitution format. The launchd template additionally requires XML
escaping for `&`, `<`, and `>`; choosing ordinary absolute paths avoids that issue.
The `newsyslog` configuration is whitespace-delimited, so its log directory must not
contain whitespace. Install `wikisync-log-maintenance.sh` unchanged and user-owned;
the companion invokes it explicitly with `/bin/sh`.

The primary launchd agent writes two private files. The companion checks them hourly,
and when either reaches 10 MiB it unloads the primary agent, waits up to ten minutes
for cooperative shutdown, evaluates both streams with user-scoped `newsyslog`, retains
four gzip archives per stream, and loads the primary agent again. Stopping first is
required: launchd owns the inherited output descriptors, so renaming a live file would
leave the daemon writing into an archived inode. The 10 MiB threshold is not a hard
disk quota; one interval's output can overshoot it. Operators needing a hard ceiling
must also monitor or quota the containing filesystem.

`wikisyncd.service.in` is the persistent Linux service. The health service/timer is
optional and only runs the local `health` command every 15 minutes. It is not a sync
schedule, does not contact MediaWiki, and does not restart an intentionally disabled
daemon. Explicit GUI/CLI requests can forward synchronization, verification, and
compaction through the daemon. Synchronization schedules are durable library
configuration edited in the GUI; they are not service-manager timers.
The Linux units write to journald and apply per-unit message-rate limits. Journal
storage and age limits are administrator-owned, generally global settings; the user
unit does not claim to set or verify them. Review the documented journald policy before
leaving an unattended service enabled.

See [service-management.md](../docs/operations/service-management.md) for a cautious,
manual installation procedure. Packaging automation should perform the same token
validation and install user-owned files with mode `0600`.
