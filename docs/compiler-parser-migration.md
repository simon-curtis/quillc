# Compiler parser migration

## Migration status

The compiler frontend now uses `ra_ap_syntax` 0.0.358. `quill-syntax` builds
Quill's existing `Parse` model and lowers supported function bodies from
rust-analyzer syntax nodes; `quill-cfg` evaluates attributes from those same
nodes. Syn and proc-macro2 are development dependencies for the parser
benchmark and test-only differential oracle. The compiler does not use Syn's
AST or visitors in its production parsing path.

The public `parse_ast_with_cfg_and_edition` API is unchanged. The parse-cache
key and query-cache schema were bumped so stored results from the old parser
are discarded. `parse()` still builds the public lexer's token trees before
using the rust-analyzer collector for semantic items; the compiler driver uses
`parse_ast` and does not build that token tree.

## Parser-only performance evidence

Run the release-mode benchmark with:

```sh
cargo bench -p quill-syntax --bench parser_compare
```

The workload matches the LSP benchmark: 14,163 lines and 430,219 bytes of
Rust 2024 source. The Syn baseline tokenizes with
`proc_macro2::TokenStream` and then calls `syn::parse2::<syn::File>`. The
rust-analyzer measurement parses the same text with `SourceFile::parse`,
producing a lossless syntax tree. Both parsers accept the workload without
syntax errors. The benchmark also compares one ranged edit:
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

## Validation and limits

The current `cargo test -p quill-syntax --lib` run passes 44 tests, with 3
opt-in corpus tests ignored by default. Workspace source files and focused
fixtures are compared against the test-only Syn oracle, including edition
behavior, cfg filtering, byte spans, macro handling, CRLF, and malformed input.
The migration validation also recorded passing `quill-cfg` (10 tests),
`quill-query` (9 cache tests), and `cargo check -p quillc` runs.

On 2026-10-13, rustc acceptance gates checked 304 accepted fixtures among 440
complete-source parser fixtures, then checked 1,125 rustc-accepted source files
across eight installed standard-library crates. Quill's collector accepted all
inputs rustc's AST dump accepted in both gates. The commands, fixture
denominators, and limits are documented in
[`rust-syntax-coverage.md`](rust-syntax-coverage.md). These tests establish
broad measured coverage, not universal compatibility with every Rust grammar
form; malformed inputs, unstable syntax, and grammar differences still need
ongoing differential coverage.

The broader `cargo test -p quill-hir -p quill-typeck -p quill-codegen` suite
passes (111 tests). `cargo test -p quillc` currently has two failures in
borrow-checking scenarios: an `if let` move check and mutable vector calls.
The syntax, query, HIR, type-checking, and codegen test suites pass; these two
binary integration cases still need borrow-checker follow-up.

The last opt-in comparison over 38,349 Rust files from the local Cargo
registry reported 136 differences from Syn. A follow-up fixed 52 enum
collection discrepancies in generated expected-output fixtures, but the full
corpus has not been rerun after that fix. Remaining known differences include
cases where rust-analyzer rejects
some malformed or unsupported forms Syn recovered, and Syn mishandles some
valid macro argument lists. Other differences include opaque pattern syntax
inside `matches!` macro arguments. One concrete rust-analyzer grammar gap is an
item-position macro named `safe!`, which rust-analyzer can interpret as an item
modifier. The corpus therefore supports broad real-world coverage but does not
prove literal 100% Rust grammar compatibility. Run the ignored corpus test to
repeat the comparison.

The benchmark numbers above are parser-only evidence. They do not establish a
full compiler throughput improvement: an end-to-end compile benchmark has not
yet been run after the migration.
