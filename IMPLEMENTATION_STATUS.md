# WikiSyncer implementation status

Updated: 2026-08-21

## Completed backlog items

1. Cargo workspace, formatting and lint policy, macOS/Linux CI, and dependency
   license/source policy.
2. Validated core identities, explicit-title and category collection rules, and all
   planned history-policy variants.
3. Bounded MediaWiki Action API client, 100-title batching spike, resumable revision
   metadata pagination, retry classification, and offline loopback fixture harness.
4. SQLite migration 1 and a bounded atomic loose-object store with domain-separated
   BLAKE3 identities, Zstandard compression, durable installation, deduplication, and
   verified reads. Logical content identity is isolated from physical locations.
5. Explicit-title resolution and current-revision capture, including exact revision
   source retrieval, content-model/size/MediaWiki SHA-1 validation, schema migration
   2 for wiki/page/revision/collection metadata, missing-title observations, and
   idempotent durable capture.
6. Deterministic normalized plain text and minimal Markdown with output-specific
   transformer versions, conservative structural/inline wikitext handling, readable
   placeholders for unsupported templates, malformed-input bounds, and golden article
   fixtures.
7. Contentless SQLite FTS5 indexing for selected current revisions with separately
   weighted title, alias, heading, body, category, and caption fields; atomic derived
   index replacement during capture; wiki filters and bounded ranked queries; and CLI
   `search`/`show` commands with human-readable or JSON output and exact-source mode.

## Next backlog item

8. Add revision enumeration, history, and local diff.

Milestone gates remain tracked in `IMPLEMENTATION_PLAN.md`; an item being complete
means its initial implementation is present, not that its later milestone hardening
is finished.
