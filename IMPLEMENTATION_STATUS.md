# WikiSyncer implementation status

Updated: 2026-08-20

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

## Next backlog item

6. Add normalized plain text, minimal Markdown, and deterministic transformer
   versions.

Milestone gates remain tracked in `IMPLEMENTATION_PLAN.md`; an item being complete
means its initial implementation is present, not that its later milestone hardening
is finished.
