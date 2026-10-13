# Architecture and implementation boundaries

Use this guide when changing compiler stages, crate contracts, or scheduling.
For language-completion criteria, read [the compatibility target](../benchmarks/compatibility/TARGET.md).
For the editor pipeline, read [the LSP guide](lsp.md).

## Completing a vertical slice

1. State the input forms the command accepts and the cases it rejects.
2. Carry the behavior through driver input, source spans, diagnostics, semantic
   handling, and output. Each affected stage must preserve source identity.
3. Add a focused correctness case for the complete path. A slice is complete
   when its accepted and rejected cases behave as specified, not merely when a
   frontend stage recognizes the syntax.
4. Evaluate a performance claim through [the benchmark protocol](../benchmarks/README.md)
   after the correctness gate passes. A crate boundary is ready when the next
   stage can use its contract and that contract can be validated independently.

<a id="constraints"></a>
## Design constraints

- **Native output:** the first target is x86-64 Linux ELF. Quill selects
  instructions, emits bytes, and creates relocations. `object` serializes ELF;
  the system linker completes the executable. LLVM and Cranelift must remain
  outside the dependency graph and output path.
- **Comparison:** `rustc` supplies the reference behavior and target `cfg`
  values. Record its profile, target, linker, and host with comparisons.
- **Diagnostics:** check every body required by Rust, including unreachable
  code. Reachability may prune downstream monomorphization, optimization,
  lowering, and emission; it must not skip required type or borrow checking.
- **Boundaries:** keep `quillc` user-facing. Split crates around independently
  usable contracts rather than individual internal types.
- **Scope:** every command states and enforces its current subset. Staging
  implementation does not narrow the compatibility destination.

## Target crate boundaries

This table describes intended responsibilities. Read the workspace manifest
for current membership: `quill-cfg` exists; `quill-mir` and `quill-object` are
not yet separate crates.

| Boundary | Contract |
| --- | --- |
| `quill-source` | Immutable files, interned identities, byte spans, and line/column mapping. |
| `quill-syntax` | Tokens, parsing, and source-spanned diagnostics. |
| `quill-hir` | Resolved items and bodies with source identity and stable item keys. |
| `quill-query` | Memoized immutable results, dependencies, persistent fingerprints, and invalidation. |
| `quill-typeck` | Inference, trait solving, and borrow checking; shared generic definitions and demanded instances deduplicated by canonical key. |
| `quill-mir` | Explicit control flow and per-body optimization. |
| `quill-codegen` / `quill-object` | Low-level IR, x86-64 lowering, register allocation, and ELF emission. |
| `quill-link` | System-linker invocation; replace only if end-to-end measurements identify a bottleneck. |
| `quill-cargo` | Workspace metadata, target selection, dependency planning, and compiler integration. |

## Current implementation boundary

### Frontend and Cargo

The driver parses compiler source with rust-analyzer's `ra_ap_syntax` AST and
retains a custom lexer for `lex` and token spans. `quill-cfg` reads attributes
from the same syntax tree. A test-only Syn oracle checks the existing Quill
model during migration. The compiler discovers conventional and supported
`#[path]` modules, applies supported target/feature `cfg`, and parses
independent files in parallel.
HIR records module/target ownership, imports, struct fields, impl ownership,
and selected qualified calls. Complete Rust name resolution, visibility, type
namespaces, and general macro expansion remain incomplete. Exact supported
forms belong in the [feature inventory](../compatibility.html), not a second
feature list here.

`quill-cargo` uses `cargo metadata` for packages, default members, features,
and normal/build dependency edges:

| Command | Current role |
| --- | --- |
| `cargo-plan` | Report dependency waves. |
| `cargo-parse` | Discover target module trees, build per-target HIR, and resolve supported public calls through workspace dependency aliases. |
| `cargo-check-subset` | Check the primitive subset using local and workspace signatures. |
| `cargo-build-subset` | Build supported default-member binaries with at most one same-package library and workspace-local ordinary library dependencies. |

A single selected binary uses the requested output file. Multiple binaries use
matching names in the requested output directory; `--bin` selects one. Output
is ELF executables, not Cargo-compatible `.rlib` files. External dependencies
and general build-script support remain incomplete; consult the inventory for
supported exceptions and the [corpus](../benchmarks/compatibility/README.md)
for observed behavior.

### Queries and scheduling

Parse keys include canonical path, content, and `cfg` fingerprint. Whole-file
and per-body type-check entries use source and callable-interface fingerprints,
with separate strict and permissive results. Struct interfaces participate in
invalidation; cross-file semantic invalidation is still incomplete.

`quill-parallel` uses a bounded Rayon work-stealing pool and preserves diagnostic
order. Dependency-driven continuation suspension/resumption and wait-for cycle
detection remain planned. Reusing cached work must preserve the diagnostic
contract above.

### Checking and output

The checker and backend implement selected combinations of scalar operations,
control flow, structs, generics, references, collections, iterator closures,
functions, tuples, strings, slices, enums, and `Option`/`Result` layouts. They
do not provide complete Rust type/borrow behavior, ABI, allocator, panics, or
standard-library semantics.

Catalog Flow demonstrates a supported end-to-end path with output matching
`rustc`. Quill emits one executable; Cargo also produces per-crate artifacts.
Read [the paired report](../benchmarks/catalog-flow/RESULTS.md#scope) before
using it as performance or compatibility evidence.

<a id="scientific-method"></a>
## Benchmark method

Use [the benchmark protocol](../benchmarks/README.md#acceptance-gate) for the
correctness, throughput, and memory gates. Keep a single hypothesis per change
and preserve raw rows. Isolated frontend profiles and overlapping timing ranges
cannot establish a general compiler speedup.

## Research references

- [rust-analyzer syntax crate](https://docs.rs/ra_ap_syntax/latest/ra_ap_syntax/)
- [Rust compiler query system](https://rustc-dev-guide.rust-lang.org/query.html)
- [Rust parallel compiler](https://rustc-dev-guide.rust-lang.org/parallel-rustc.html)
- [Rust incremental compilation](https://rustc-dev-guide.rust-lang.org/queries/incremental-compilation.html)
- [Go compiler pipeline](https://go.dev/src/cmd/compile/README)
- [Zig incremental compilation](https://ziglang.org/devlog/2024/)
- [Odin releases](https://github.com/odin-lang/Odin/releases)
- [Blumofe and Leiserson, randomized work stealing](https://dl.acm.org/doi/10.1145/324133.324234)
