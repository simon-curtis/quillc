# Quill

Quill is an experimental Rust compiler written in Rust. The goal is full Rust
compatibility on Linux and Windows, with at least 3× the pinned `rustc` build
throughput and no increase in peak memory. These are acceptance targets;
[benchmark reports](benchmarks/README.md) describe measured results.

## Start here

- **Install on Arch Linux:** [signed package and source build](docs/arch-packaging.md).
- **Work on the compiler:** [architecture and implementation boundaries](docs/architecture.md).
- **Use the editor server:** [LSP setup and behavior](docs/lsp.md).
- **Run the website:** [Vite+ development workflow](docs/website/README.md).
- **Evaluate compatibility:** [full target](benchmarks/compatibility/TARGET.md),
  [feature inventory](compatibility.html), and [differential corpus](benchmarks/compatibility/README.md).
- **Measure performance:** [protocol](benchmarks/README.md) and
  [Catalog Flow results](benchmarks/catalog-flow/RESULTS.md).

Use the toolchain in [rust-toolchain.toml](rust-toolchain.toml) for repository
work. [AGENTS.md](AGENTS.md) routes implementation tasks to their detailed guides.

<a id="current-state"></a>
## Current implementation

The native backend targets x86-64 Linux ELF and compiles a growing subset of
Rust. Quill emits its own machine code and relocations; `object` serializes ELF
objects and the system linker links them. LLVM and Cranelift are not backends
or dependencies.

The compiler frontend, caches, scheduler, and Cargo integration are described
in [architecture](docs/architecture.md#current-implementation-boundary). The
editor uses a separate full-fidelity parser; its [syntax contract](docs/rust-syntax-coverage.md)
does not imply compiler semantic support.

`cargo-build-subset` builds the Catalog Flow fixture with output matching
`rustc`. That result applies to the fixture and documented command subset.
Type and borrow checking, trait solving, macros, libraries, Cargo integration,
and artifact compatibility still have gaps. Consult the [feature inventory](compatibility.html)
for the exact supported forms and remaining limits.

## Limits and evidence

Read each measurement with its workload, compiler state, and artifact scope.
Build throughput, generated-program runtime, editor latency, parser coverage,
and Rust conformance are separate results. We expect generated-program runtime
to lag LLVM-backed `rustc`; faster builds do not imply faster executables.
