# macOS signing and notarization gate

This document defines the macOS release gate without claiming that WikiSyncer has a
provisioned production Developer ID identity or an accepted notarized release today.
Routine CI validates native Mach-O inputs and the orchestration policy without using
credentials, contacting Apple, signing code, or publishing artifacts.

Apple's current distribution guidance requires Developer ID command-line executables
to use the hardened runtime and a secure timestamp. The Apple notary service accepts
ZIP, PKG, and DMG submissions; the current WikiSyncer distribution is a `tar.gz`,
which cannot carry a stapled ticket. See Apple's documentation for
[distribution signing](https://developer.apple.com/documentation/xcode/creating-distribution-signed-code-for-the-mac/),
[Developer ID and notarization](https://developer.apple.com/developer-id/), and
[common notarization failures](https://developer.apple.com/documentation/security/resolving-common-notarization-issues).

## Credential-free preparation

The native macOS dry run generates a deterministic plan with an unmistakable
all-zero certificate fingerprint and an unprovisioned identity:

```sh
python3 packaging/scripts/release.py macos-signing-plan \
  --input-dir target/release \
  --output-file target/release-candidate/macos-signing-plan.json \
  --version 0.1.0 \
  --target-arch aarch64 \
  --signing-identity \
    'Developer ID Application: UNPROVISIONED WIKISYNC RELEASE (AAAAAAAAAA)' \
  --team-id AAAAAAAAAA \
  --certificate-sha1 0000000000000000000000000000000000000000 \
  --source-date-epoch 1700000000 \
  --credential-free-dry-run
```

The planner selects only the three expected executable names and requires regular,
executable, structurally bounded Mach-O containers containing the requested
architecture. It validates thin/fat headers, slice ranges and overlap, per-slice CPU
types, executable file type, and load-command bounds; this is early corruption defense,
not a substitute for Apple's native signing validation. The planner also requires an
exact Developer ID Application authority shape, a matching ten-character Team ID, and
a forty-digit certificate fingerprint. Its canonical JSON records the pre-signing
SHA-256 and size, fixed code identifiers, and argument arrays for hardened-runtime,
secure-timestamp signing and strict verification. The all-zero fingerprint is rejected
unless the explicit dry-run flag is present; a nonzero fingerprint is rejected with
that flag.

The dry-run identity is not trusted and must never appear in a production plan. A
plan is a reviewed procedure and input inventory, not a signature, notarization
receipt, provenance attestation, or authorization to use credentials.

## Authorized native release sequence

An authorized release operator must perform these steps on a protected native macOS
host. The ordinary pull-request workflow deliberately cannot perform them.

1. Resolve the intended Developer ID Application certificate in the login keychain.
   Record its full leaf authority, ten-character Team ID, and SHA-1 certificate hash.
   Use the hash with `codesign --sign` so duplicate certificate names are not
   ambiguous. Do not run signing through `sudo`.
2. Generate a production plan without `--credential-free-dry-run`. Review the three
   pre-signing hashes, identifiers, architecture, identity, Team ID, and certificate
   hash before allowing key access.
3. Execute the plan's three signing argument arrays against an isolated copy of the
   binaries. Signing is an external write and `--timestamp` contacts Apple's timestamp
   service; it therefore requires explicit release authorization and network policy.
   No entitlements are planned for these command-line executables.
4. Validate the signed copy before packaging:

   ```sh
   python3 packaging/scripts/release.py verify-macos-signatures \
     --input-dir target/signed \
     --target-arch aarch64 \
     --signing-identity 'Developer ID Application: PUBLISHER (TEAMID1234)' \
     --team-id TEAMID1234
   ```

   The validator runs Apple's fixed `/usr/bin/codesign`, never a PATH-selected test
   substitute. For every executable it verifies all Mach-O architectures and requires
   the exact leaf authority, the Developer ID/Apple root chain, exact Team ID and code
   identifier, the hardened-runtime flag, and exactly one secure `Timestamp` rather
   than an unauthenticated `Signed Time` on each architecture.
5. Put the signed executables in a ZIP, PKG, or DMG submission payload. Record the
   payload's SHA-256 before submission. Submit that exact payload with `xcrun
   notarytool submit --wait --output-format json` using a protected keychain profile;
   never pass App Store Connect private-key contents or passwords on the command line,
   in environment dumps, or through CI logs.
6. Record the submission UUID out of band and validate the saved JSON result:

   ```sh
   python3 packaging/scripts/release.py validate-notarization-receipt \
     --receipt protected/notary-result.json \
     --submission-id 8d96bd1d-37af-45d1-9dcd-27d12db9cc12
   ```

   This check requires a bounded regular JSON file, the exact expected UUID, and
   status `Accepted`. The JSON file is operator evidence, not a consumer trust anchor
   and not cryptographic proof that some other local payload was accepted. Retain the
   submitted payload hash and Apple log with the protected release record.
7. Build the published `tar.gz` from those exact signed binaries, then bind its payload
   back to the validated signing inputs:

   ```sh
   python3 packaging/scripts/release.py verify-macos-release-archive \
     --input-dir target/signed \
     --archive target/release-candidate/wikisync-0.1.0-macos-aarch64.tar.gz \
     --target-arch aarch64 \
     --signing-identity 'Developer ID Application: PUBLISHER (TEAMID1234)' \
     --team-id TEAMID1234
   ```

   This reruns the signature policy and archive-layout checks, requires macOS release
   metadata, and compares every archived executable byte-for-byte by SHA-256 with the
   validated signed input set. Because a tar archive cannot be stapled, validate
   notarization assessment on a clean supported macOS system with normal Gatekeeper
   policy and network access before publication. If offline stapled validation becomes
   a release requirement, switch the macOS distribution container to a supported,
   reviewed PKG or DMG design; do not claim that the tar archive is stapled.
8. Only after the native gate succeeds, create and independently verify the protected
   WikiSyncer signature over `SHA256SUMS`. Publish the archives, checksum manifest,
   and detached signature as one reviewed set.

## Remaining credentialed evidence

Before calling a beta archive signed and notarized, the project still needs a real
Developer ID Application identity and protected certificate lifecycle, a protected
notarytool credential profile, an accepted submission and retained Apple log, exact
signed-input-to-published-archive comparison, clean-machine Gatekeeper assessment,
and a reviewed publication run. None of those facts can be inferred from the
credential-free plan or test fixtures.

Code signing authenticates the publisher-authorized executable bytes. Notarization
adds Apple's automated service assessment. Neither proves source truth, dependency or
compiler integrity, reproducible correspondence to source, or the integrity of a
user's WikiSyncer library.
