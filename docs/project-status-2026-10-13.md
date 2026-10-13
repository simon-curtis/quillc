# Quill status — 2026-10-13

## Goal and current limits

Quill remains an experimental, incomplete Rust compiler. It targets full Rust
compatibility and at least 3× build throughput without higher peak memory, but
neither target is met generally. Its native x86-64 Linux backend emits machine
code and ELF objects directly; LLVM and Cranelift are not compiler backends or
Quill dependencies. Cargo-compatible Rust library metadata and broad external
crate support are still missing.

The pinned 30,992-line Catalog Flow fixture showed more than 3× faster wall and
CPU time with equal or lower peak memory in cold, warm, incremental, and
semantic-incremental modes. These measurements apply only to that fixture,
host, toolchain, and command set. They do not demonstrate whole-Rust
compatibility or a general 3× compiler advantage. See
[the benchmark protocol](../benchmarks/README.md) and
[paired results](../benchmarks/catalog-flow/RESULTS.md).

The curated compatibility run recorded 57/58 cases. The remaining case needs a
Rust library artifact containing rustc-compatible metadata for `rustc
--extern`; a Quill object file is not sufficient. The separate syntax and LSP
checks measure parser behavior and editor latency, not type-system or compiler
compatibility. See the [compatibility corpus](../benchmarks/compatibility/README.md)
and [syntax coverage](rust-syntax-coverage.md).

## Validation in this checkout

- `cargo +nightly-2026-08-23 test -p quill-lsp -p quill-syntax`: passed (4 LSP
  tests and 43 syntax tests; 3 differential probes are ignored by default).
- `cargo +nightly-2026-08-23 test -p quill-typeck -p quillc`: currently fails.
  Four type-checker cases fail in the dirty projected-reborrow/trait-context
  work: opaque `Into<String>` handling, external self-returning builder methods,
  `AsRef` in tuple/slice loops, and mutable reborrow through a tuple projection.
  This work is not committed as complete.

The workspace also has two untracked ELF outputs (`crates/quillc/unused-output`
and `quill_layout_probe`) and multiple linked temporary worktrees. They must be
identified and preserved or removed deliberately; they are not source files.

## Branch and worktree cleanup

`main` contains the integrated `fix/complete-lsp-build-dependency` work. The
local compatibility and experiment branches include divergent commits; they
have not all been merged into `main`. Several detached worktrees also remain.
Do not delete them until their unique commits and uncommitted files have been
reviewed. The compiler task is not complete, so this note records the handoff
state rather than claiming completion.
