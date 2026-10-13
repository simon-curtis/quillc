# Compiler parser migration

## Decision

Keep Syn as the compiler's AST and lowering input until an equivalent
rust-analyzer AST adapter exists. Use `ra_ap_syntax` as the LSP syntax parser
and keep the parser comparison benchmark as the migration performance gate.

The compiler parser is not a single call that can be swapped. `quill-syntax`
parses into `syn::File`, then `AstCollector` visits Syn nodes to build Quill's
`Parse` model and lower function bodies. `quill-cfg` separately uses Syn's
`Meta` tree to evaluate attributes. The query cache calls
`parse_ast_with_cfg_and_edition` and persists the resulting Quill model.
Replacing the parser therefore requires an adapter from rust-analyzer syntax
nodes to the same Quill model, plus a separate migration for cfg metadata.

## Parser-only performance evidence

Run the release-mode benchmark with:

```sh
cargo bench -p quill-syntax --bench parser_compare
```

The workload matches the LSP benchmark: 14,163 lines and 430,219 bytes of
Rust 2024 source. Syn is timed through Quill's current parser operation:
`proc_macro2::TokenStream` tokenization followed by `syn::parse2::<syn::File>`.
The rust-analyzer measurement parses the same text with
`SourceFile::parse`, producing a lossless syntax tree. Both parsers accept the
workload without syntax errors. The benchmark also compares one ranged edit:
rust-analyzer incrementally reparses from the prior tree while Syn reparses the
edited file from scratch.

Three independent 31-round release runs produced these median-of-run medians:

| Operation | rust-analyzer | Syn | Comparison |
| --- | ---: | ---: | ---: |
| Full source parse | 26.308 ms | 41.009 ms | rust-analyzer 1.56x as fast |
| One edit after initial parse | 8.693 ms | 42.251 ms | rust-analyzer 4.86x as fast |

The raw three-run samples are in
[`../benchmarks/results/parser-comparison-20261013.json`](../benchmarks/results/parser-comparison-20261013.json).

The benchmark is parser-only. It does not include Syn's `AstCollector`, Quill
body lowering, cfg evaluation, query caching, or compilation. Treat these
numbers as evidence for the parser layer, not a compiler throughput claim. The
checked-in benchmark prints all samples and p50/p95 values for reruns on the
target machine.

## Migration sequence

1. Add a rust-analyzer backed collector that returns the existing Quill
   `Parse` model from `SourceFile` nodes. Keep the public
   `parse_ast_with_cfg_and_edition` contract stable during this work.
2. Add a test-only differential mode that runs both collectors over the
   compiler's supported-source fixtures and compares item models, spans,
   diagnostics, cfg behavior, and edition behavior.
3. Port body lowering and cfg metadata deliberately. Keep Syn for any custom
   token or metadata parsers that do not yet have a parity-tested replacement.
4. Switch the compiler default only after differential tests pass and the
   end-to-end compile benchmark has no material regression. Keep the Syn path
   available through the migration period for bisecting behavior changes.
5. Remove Syn only after the compiler no longer depends on Syn AST nodes,
   visitors, or attribute parsing.

Adding a second full parser to the production compiler before the collector is
ready would duplicate work. The benchmark depends on `ra_ap_syntax` only as a
development dependency, so this benchmark adds no production dependency edge
from `quill-syntax`.
