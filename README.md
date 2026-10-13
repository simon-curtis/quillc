# Quill

Quill is an experimental Rust compiler written in Rust. Its long-term targets
are Rust compatibility and at least 3× the pinned `rustc` throughput on the
same workloads without higher peak memory. These are acceptance goals; only
the results in the benchmark reports are measured.

The workspace uses the pinned toolchain in `rust-toolchain.toml`.

## Arch Linux

Quill is available as the signed `quillc` pacman package, starting at version
`0.0.1`. See [Arch installation and packaging](docs/arch-packaging.md) for the
public signing key, repository configuration, and build commands.

Quill owns its x86-64 code generator. LLVM and Cranelift are not dependencies
or backends. The `object` crate serializes Quill's machine code and
relocations, and the system linker links the resulting ELF objects.

## Current implementation

The workspace has source, syntax, HIR, query, cfg, type-checking, parallel,
code-generation, linking, and Cargo integration crates, plus the `quillc`
driver. The frontend uses Syn to parse Rust files, discovers module trees,
builds partial HIR, and caches parse and type-check results. Cargo commands
read workspace metadata, apply supported target and feature `cfg` predicates,
and resolve some public calls across workspace packages.

The native backend targets x86-64 Linux ELF and supports a growing subset of
Rust syntax and runtime behavior. `cargo-build-subset` builds the checked-in
`catalog-flow` workspace into one executable. Its output matches the `rustc`
binary for that fixture. This demonstrates the measured fixture only; it does
not establish general Rust or Cargo compatibility.

## Limits and evidence

This is not a full Rust compiler. Type and borrow checking, trait solving,
macros, standard-library behavior, external dependencies, build scripts,
procedural macros, and Cargo artifact formats are incomplete or unsupported.
Each command accepts only its documented subset; unsupported input is not
evidence of compiler compatibility.

See [the architecture and current boundaries](docs/architecture.md) for the
design and implementation scope. See [the benchmark protocol](benchmarks/README.md)
and [catalog-flow results](benchmarks/catalog-flow/RESULTS.md) for reproduction
steps, measurements, and their limits.
