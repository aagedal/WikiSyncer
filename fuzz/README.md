# WikiSyncer fuzz targets

These maintained targets exercise bounded, untrusted inputs through the same public
APIs used by the product:

- `wikitext_markdown`: deterministic wikitext-to-text/Markdown rewriting and diffs;
- `trusted_head_json`: strict canonical JSON parsing and re-encoding;
- `dump_parser`: bounded bzip2/XML dump parsing;
- `loose_object`: bounded Zstandard loose-object decompression and identity checks;
- `pack_roundtrip`: pack/index decoding, pruning, bounded delta reconstruction, and
  repacking while preserving every logical object;
- `action_api_json`: bounded validation of every untrusted MediaWiki Action API
  JSON response shape used by title, revision, category, and image ingestion.

Maintained sample corpora live under `fuzz/corpus/<target>/`. They include empty,
short, Unicode, nested-structure, and deliberately incomplete inputs where those
cases apply. Compressed targets include valid format samples as well as truncated or
invalid representations, so both decoding and rejection paths receive seed coverage.

Install the nightly Rust toolchain and `cargo-fuzz`, then run one target from the
repository root. Keep the workspace's default toolchain on stable and select nightly
only for the instrumented fuzz command:

```sh
rustup toolchain install nightly --profile minimal
cargo +nightly fuzz run wikitext_markdown -- -max_len=262144
cargo +nightly fuzz run trusted_head_json -- -max_len=4097
cargo +nightly fuzz run dump_parser -- -max_len=262144
cargo +nightly fuzz run loose_object -- -max_len=65536
cargo +nightly fuzz run pack_roundtrip -- -max_len=32768
cargo +nightly fuzz run action_api_json -- -max_len=262144
```

The 256 KiB Action API command is the fast routine campaign. The harness itself
accepts the full production response-body ceiling: 8 MiB of JSON plus its one-byte
response-kind selector. Generate deterministic near-ceiling inputs without adding
large fixtures to the repository, then use them for an explicitly resource-sized
campaign:

```sh
python3 fuzz/scripts/generate_action_api_corpus.py \
  --output fuzz/target/action-api-production-corpus
cargo +nightly fuzz run action_api_json \
  fuzz/target/action-api-production-corpus -- \
  -max_len=8388609 -max_total_time=600
```

The generator derives all seven response kinds from their maintained valid seeds.
Each JSON body is exactly 8 MiB, with the space carried by a response field consumed
by that operation's production decoder (including exact revision content, comments,
titles, image references, and attribution). Additional inputs exercise the typed
50-page, 500-revision, 500-category-member, and 4,096-image-reference limits. Pass a
smaller `--body-size` for quick resource sizing; the generator rejects sizes above
the production ceiling. It only accepts an empty directory or one containing its
own named outputs, so use a fresh directory when restarting after libFuzzer has
expanded the working corpus. Its SHA-256/length listing makes a campaign input set
recordable and reproducible.

Generation and a bounded seed smoke run establish reproducible campaign inputs; they
are not sustained fuzz evidence by themselves. Record the duration, executions,
coverage, peak RSS, toolchain, corpus digests, and outcome of the actual resource-
sized campaign before treating that release-acceptance work as complete.

Validate the generator itself with only the Python standard library:

```sh
python3 -m unittest fuzz/scripts/test_generate_action_api_corpus.py -v
```

For a short local smoke run against each maintained corpus:

```sh
cargo +nightly fuzz run wikitext_markdown fuzz/corpus/wikitext_markdown -- -runs=100 -max_len=262144
cargo +nightly fuzz run trusted_head_json fuzz/corpus/trusted_head_json -- -runs=100 -max_len=4097
cargo +nightly fuzz run dump_parser fuzz/corpus/dump_parser -- -runs=100 -max_len=262144
cargo +nightly fuzz run loose_object fuzz/corpus/loose_object -- -runs=100 -max_len=65536
cargo +nightly fuzz run pack_roundtrip fuzz/corpus/pack_roundtrip -- -runs=25 -max_len=32768
cargo +nightly fuzz run action_api_json fuzz/corpus/action_api_json -- -runs=100 -max_len=262144
```

The explicit `-max_len` values complement the limits in each harness. The storage
targets intentionally use small temporary libraries; run them separately from the
fast pure transformation targets. Crash artifacts and local build output are
ignored. `action_api_json` reserves its first input byte as a deterministic response
kind selector (`T`, `H`, `B`, `C`, `M`, `I`, or `N` for title resolution, page head,
revision batch, revision content, category members, revision images, or thumbnail
metadata respectively); all other selector bytes are mapped across the same seven
kinds. The remaining bytes are the untrusted JSON body and may reach the production
8 MiB response ceiling even when a smaller routine-campaign `-max_len` is selected.
