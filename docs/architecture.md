# Architecture and implementation boundaries

## Design constraints

- The first native target is x86-64 Linux ELF. Quill selects instructions,
  emits machine code, and creates relocations itself. LLVM and Cranelift must
  not enter its dependency graph or output path. `object` serializes ELF data;
  `ld` links it.
- `rustc` is a comparison compiler and a source of target `cfg` values. Report
  its profile, target, linker, and machine settings with every comparison.
- Rust compatibility is the destination, not the bootstrap contract. Each
  command must state and enforce its supported subset.
- Check every body required by Rust diagnostics, including unreachable code.
  Reachability may remove downstream monomorphization, optimization, lowering,
  and emission work; it must not skip required type or borrow checking.
- Keep the `quillc` driver user-facing. Split crates when they expose a useful
  contract that can be built or measured independently, not for each internal
  type.

## Target crate boundaries

These are architectural boundaries, not a claim that each package exists
today. The workspace currently includes `quill-cfg`; `quill-mir` and
`quill-object` are not separate crates yet.

1. `quill-source`: immutable source files, interned file identities, byte
   spans, and line/column mapping.
2. `quill-syntax`: tokens, parsing, and source-spanned diagnostics.
3. `quill-hir`: resolved items and bodies with source identity and stable item
   keys.
4. `quill-query`: memoized immutable results, dependency tracking, persistent
   fingerprints, and invalidation.
5. `quill-typeck`: type inference, trait solving, and borrow checking. Keep
   generic definitions shared; instantiate only demanded uses and deduplicate
   by canonical instance key.
6. `quill-mir`: explicit control flow and per-body optimization.
7. `quill-codegen` and `quill-object`: target-independent low-level IR,
   x86-64 lowering, register allocation, and ELF object emission, without LLVM
   or Cranelift interfaces.
8. `quill-link`: system-linker invocation. Replace it only if end-to-end
   measurements identify linking as a bottleneck.

`quill-cargo` is the Cargo integration boundary. It uses `cargo metadata` to
resolve packages, default members, features, and normal/build dependency
edges. `cargo-plan` reports dependency waves; `cargo-parse` discovers module
trees, parses target roots, builds per-target HIR, and resolves supported
public calls through workspace dependency aliases. `cargo-check-subset` adds
primitive checks using local and workspace signatures. `cargo-build-subset`
can compile a supported binary target with at most one same-package library
and workspace-local ordinary library dependencies into one ELF executable.
It does not emit Cargo-compatible `.rlib` files. External dependencies in the
selected closure, build scripts, and multiple binary targets are unsupported.

## Current implementation boundary

The driver parses files with Syn and retains a custom lexer for the `lex`
command and token spans. It discovers conventional external modules and
supported `#[path]` modules, evaluates supported target/feature `cfg`
predicates, and parses independent files in parallel. HIR retains module and
target ownership, imports, struct fields, impl ownership, and selected
qualified call paths. It does not expand macros or implement Rust's complete
name resolution, visibility, or type namespaces. The frontend constant-folds
literal-only `concat!` invocations; general declarative and procedural
macro expansion remains unsupported.

Parse queries are keyed by canonical path, content, and `cfg` fingerprint.
Type-check queries have whole-file and per-body cache entries; body entries
include source and callable-interface fingerprints, with separate strict and
permissive results. Struct field interfaces participate in invalidation.
Cross-file semantic invalidation is still incomplete. `quill-parallel` uses a
bounded Rayon work-stealing pool for independent file and body work and
preserves diagnostic order. Semantic queries do not yet suspend and resume
continuations on dependencies, and wait-for cycle detection remains planned.

The checker accepts a primitive subset, including selected scalar operations,
control flow, structs, generics, references, collections, and iterator
closures. Passing a checker command does not establish Rust type or borrow
compatibility. The x86-64 backend supports selected expressions, loops,
functions, tuples, strings, slices, enums, `Option`/`Result` layouts, and
collection operations. Those features work only in documented combinations;
the backend does not provide the full Rust ABI, allocator, panic behavior, or
standard-library runtime.

`cargo-build-subset` currently builds and runs the `catalog-flow` fixture with
output matching `rustc`. Its output is one executable while `cargo build`
produces per-crate artifacts. Read [the paired report](../benchmarks/catalog-flow/RESULTS.md)
before using this fixture as performance or compatibility evidence.

## Completing a vertical slice

Add behavior through an end-to-end path: driver input, source spans,
diagnostics, semantic handling, output, and a focused correctness case. State
the accepted syntax and rejected cases. Measure performance only after the
correctness gate passes. A crate boundary is ready when its contract can be
used by the next stage and independently validated.

## Benchmark method

For each optimization, record one hypothesis, a correctness gate, one code
change, repeated cold/warm/incremental measurements, and the compiler revision.
Keep raw rows. Reject a claimed gain when behavior differs, peak memory exceeds
the target, or ranges do not support the conclusion. The 3× goal is an
acceptance target: every required workload and build mode must pass the
behavior check, reach at least 3× median wall-time speedup over pinned
`rustc`, and use no more peak memory.

See [the benchmark protocol](../benchmarks/README.md) for the workload,
measurement method, and experiment index. One synthetic workload cannot
establish broad Rust compatibility. Later gates need diverse real workspaces,
Cargo profiles, diagnostics, and executable behavior comparisons.

## Research references

- [Syn file parser](https://docs.rs/syn/latest/syn/fn.parse_file.html)
- [Rust compiler query system](https://rustc-dev-guide.rust-lang.org/query.html)
- [Rust parallel compiler](https://rustc-dev-guide.rust-lang.org/parallel-rustc.html)
- [Rust incremental compilation](https://rustc-dev-guide.rust-lang.org/queries/incremental-compilation.html)
- [Go compiler pipeline](https://go.dev/src/cmd/compile/README)
- [Zig incremental compilation](https://ziglang.org/devlog/2024/)
- [Odin releases](https://github.com/odin-lang/Odin/releases)
- [Blumofe and Leiserson, randomized work stealing](https://dl.acm.org/doi/10.1145/324133.324234)
