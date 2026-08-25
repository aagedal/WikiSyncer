# WikiSyncer fuzz targets

These maintained targets exercise bounded, untrusted inputs through the same public
APIs used by the product:

- `wikitext_markdown`: deterministic wikitext-to-text/Markdown rewriting and diffs;
- `trusted_head_json`: strict canonical JSON parsing and re-encoding;
- `dump_parser`: bounded bzip2/XML dump parsing;
- `loose_object`: bounded Zstandard loose-object decompression and identity checks;
- `pack_roundtrip`: pack/index decoding, pruning, bounded delta reconstruction, and
  repacking while preserving every logical object.

Maintained sample corpora live under `fuzz/corpus/<target>/`. They include empty,
short, Unicode, nested-structure, and deliberately incomplete inputs where those
cases apply. Compressed targets include valid format samples as well as truncated or
invalid representations, so both decoding and rejection paths receive seed coverage.

Install `cargo-fuzz`, then run one target from the repository root, for example:

```sh
cargo fuzz run wikitext_markdown -- -max_len=262144
cargo fuzz run trusted_head_json -- -max_len=4097
cargo fuzz run dump_parser -- -max_len=262144
cargo fuzz run loose_object -- -max_len=65536
cargo fuzz run pack_roundtrip -- -max_len=32768
```

For a short local smoke run against each maintained corpus:

```sh
cargo fuzz run wikitext_markdown fuzz/corpus/wikitext_markdown -- -runs=100 -max_len=262144
cargo fuzz run trusted_head_json fuzz/corpus/trusted_head_json -- -runs=100 -max_len=4097
cargo fuzz run dump_parser fuzz/corpus/dump_parser -- -runs=100 -max_len=262144
cargo fuzz run loose_object fuzz/corpus/loose_object -- -runs=100 -max_len=65536
cargo fuzz run pack_roundtrip fuzz/corpus/pack_roundtrip -- -runs=25 -max_len=32768
```

The explicit `-max_len` values complement the limits in each harness. The storage
targets intentionally use small temporary libraries; run them separately from the
fast pure transformation targets. Crash artifacts and local build output are
ignored.
