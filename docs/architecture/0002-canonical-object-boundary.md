# ADR 0002: Separate canonical identity from physical storage

- Status: accepted
- Date: 2026-08-20

## Context

Captured revision bytes must remain addressable and verifiable while their on-disk
representation evolves from loose compressed objects to full or delta pack entries.

## Decision

A revision refers only to a versioned, domain-separated BLAKE3 content-object ID.
Physical location, compression, pack generation, and delta base are separate storage
metadata. Canonical wikitext is never rewritten; all reader and export formats are
rebuildable derived data.

New objects become durable through a verified temporary write and atomic rename
before a database transaction may reference them. Pack activation must preserve the
same logical object IDs and keep the previous representation valid until activation
is durable.

## Consequences

- Repacking cannot change revision identity or integrity manifests.
- Loose-object capture can ship before pack compaction.
- Store APIs must not expose pack offsets as logical identifiers.

