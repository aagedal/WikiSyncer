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
of identity. Always verify the signature before the checksums, then verify all
checksum entries and each archive layout.

Platform release gates remain credentialed manual work:

- On macOS, sign all Mach-O executables with an explicit Developer ID Application
  identity, assess the signatures and hardened-runtime policy, notarize the final
  distribution artifact with protected App Store Connect credentials, staple where
  the format permits, and validate with Gatekeeper on a clean supported system.
- On Linux, establish and document the project package-signing identity and repository
  trust/distribution model before calling an archive a signed Linux package. The SSH
  checksum signature authenticates the reviewed archive set; it is not an RPM/DEB
  repository signature.

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
- `@DOCUMENTATION_DIRECTORY@`: absolute path to `docs/operations` (systemd only).

Do not put shell syntax, environment variables, `~`, XML entities, or systemd `%`
specifiers in a replacement. Service managers do not perform ordinary shell
expansion here. Paths containing `@`, a newline, or a double quote are not supported
by this simple substitution format. The launchd template additionally requires XML
escaping for `&`, `<`, and `>`; choosing ordinary absolute paths avoids that issue.

`wikisyncd.service.in` is the persistent Linux service. The health service/timer is
optional and only runs the local `health` command every 15 minutes. It is not a sync
schedule, does not contact MediaWiki, and does not restart an intentionally disabled
daemon. Explicit GUI/CLI requests can forward synchronization, verification, and
compaction through the daemon. Synchronization schedules are durable library
configuration edited in the GUI; they are not service-manager timers.

See [service-management.md](../docs/operations/service-management.md) for a cautious,
manual installation procedure. Packaging automation should perform the same token
validation and install user-owned files with mode `0600`.
