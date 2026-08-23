# Linux package and repository trust

This document defines what WikiSyncer authenticates on Linux and, just as
importantly, what it does not authenticate. It is a release design, not a claim that
a production signing identity or package repository already exists.

## Current upstream artifact

The current upstream Linux distribution unit is the deterministic
`wikisync-<version>-linux-<architecture>.tar.gz` archive. It is not a `.deb`, an RPM,
or a package-manager repository. The release set has this verification chain:

```text
independently obtained allowed-signers trust anchor
  -> OpenSSH Ed25519 signature over SHA256SUMS (namespace: wikisync-release)
    -> SHA-256 entry for every release archive
      -> bounded canonical archive layout and executable payloads
```

The detached signature authenticates the exact bytes of `SHA256SUMS`; the checksums
authenticate the archives. A checksum downloaded only beside its archive detects
accidental damage but does not establish publisher identity. TLS protects transport
to a download endpoint but does not replace release signing.

Verify a downloaded directory before extracting or executing anything:

```sh
python3 packaging/scripts/release.py verify-release \
  --checksum-file ./release/SHA256SUMS \
  --signature ./release/SHA256SUMS.sig \
  --allowed-signers /trusted/wikisync-allowed-signers \
  --signer-identity release@wikisync
```

`verify-release` authenticates the signature first, then parses the authenticated
manifest, rejects any extra `wikisync-*.tar.gz` file not covered by it, verifies each
SHA-256 digest, and validates every archive's bounded canonical layout. Each archive
is copied from one securely opened descriptor to an unlinked temporary snapshot while
its digest is calculated, and layout validation reads that same snapshot rather than
reopening the download path. Its trust is no stronger than the supplied allowed-signers
file, and verification requires temporary storage up to the largest archive's size.

## Release identity and key distribution

The release-signing private key is an Ed25519 SSH key held outside source checkouts,
build outputs, online package repositories, and ordinary CI. Signing is a distinct,
credentialed publication step after native build review. The signer identity is
`release@wikisync` and the signature namespace is `wikisync-release`; both are part of
the protocol and prevent accepting the same signature in an unrelated context.

Before the first authenticated beta, the project must publish the official public key
and fingerprint through independently controlled, durable project channels. A release
directory, release notes hosted at the same download origin, or an allowed-signers
file bundled inside the archive is not an independent trust anchor. Consumers should
place a previously authenticated allowed-signers file outside the download directory
and compare a newly announced fingerprint through an independent channel before
replacing it.

Rotation requires a dated transition statement naming both old and new fingerprints,
signed by the old release identity when it is still available, an overlap period in
which releases can be verified through both anchors, and an explicit revocation path.
Loss or suspected compromise requires stopping publication; a new key announced only
from the compromised download channel must not silently inherit trust. The repository
does not contain an official production public key yet, so current dry runs and test
keys prove mechanics, not publisher identity.

## Native package-manager repositories

The SSH-signed checksum manifest does not make an APT or RPM repository trusted.
If WikiSyncer later operates native repositories, each repository is a separate
publication boundary with a dedicated OpenPGP repository key and native metadata:

- APT must authenticate `InRelease` (or `Release` plus `Release.gpg`), whose hashes
  cover the `Packages` indexes and ultimately each `.deb`. Consumers must configure a
  repository-specific keyring with a `signed-by` restriction; the key must not be
  added to a global trusted keyring.
- RPM repositories must authenticate `repomd.xml` with repository metadata checking
  enabled, retain package checksum coverage through that metadata, and verify package
  signatures with package checking enabled. The repository key must be scoped to the
  WikiSyncer repository rather than imported as an unrestricted system-wide key.

Repository signing keys should be online only to the extent required to publish
metadata and must not be the offline upstream release-signing key. Repository key
bootstrap, expiry, rotation, revocation, rollback/freeze policy, and mirror behavior
must be documented and tested before publishing installation instructions. HTTPS is
still required for availability and metadata privacy, but authenticity terminates at
the configured repository key.

A Linux distribution or other third party may rebuild WikiSyncer and sign its own
packages. In that case the distribution's package and repository keys, build policy,
and update channel are the trust root; the upstream SSH signature does not vouch for
those rebuilt bytes. Conversely, an upstream tarball authenticated by this release
process has no automatic package-manager update or rollback protection.

## Security boundary

Successful release verification establishes that the archive set matches bytes
authorized by the selected WikiSyncer release key and that its container layout meets
the local safety policy. It does not prove that the source, compiler, dependencies, CI
runner, signing workstation, or authorized release itself is benign. Reproducible
archives support comparison of equivalent native builds, but do not by themselves
provide independent reproducible-build attestation.

Library integrity is a separate trust system. Release signatures authenticate
application artifacts; library manifest signatures authenticate a selected local
archive state. Neither mechanism proves that captured Wikimedia content was true at
the source.
