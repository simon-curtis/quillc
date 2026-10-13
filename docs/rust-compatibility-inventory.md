# Rust compatibility inventory

This is the scope behind the compatibility rows on [`compatibility.html`](../compatibility.html). It records known gaps and incomplete areas, not a score or a claim that every program has been tested. A finite row count is not the size of Rust: the rows group subsystems, and each subsystem contains many rules, APIs, targets, and edge cases.

## Reference scope

The baseline is ordinary stable Rust as specified by the [Rust Reference](https://doc.rust-lang.org/reference/), for editions 2015, 2018, 2021, and 2024. The library target is the full `core`, `alloc`, and `std` API shipped by the repository's pinned toolchain. Tooling scope is the Cargo behavior documented by the [Cargo Reference](https://doc.rust-lang.org/cargo/reference/). Required platform scope is Linux x86-64 and Windows x86-64 with both MSVC and GNU environments, as recorded in [`benchmarks/compatibility/TARGET.md`](../benchmarks/compatibility/TARGET.md).

Unstable language/library features are not included in the stable compatibility target. The pinned compiler may parse or expose some unstable syntax; that is not support. Target-specific APIs and behavior belong to their target, and are not established by Linux-only results.

## Inventory groups

The compatibility page breaks the scope into these groups. The row text names important missing rules or behaviors; a row may cover a large API surface.

- **Language:** lexical forms and primitive types; expressions and operators; statements and control flow; patterns; items and modules; generics and types; references and unsafe operations; attributes; edition-specific rules; assembly and foreign declarations.
- **Checking:** name resolution, type inference, trait solving and coherence, method selection, borrow checking, lifetime checking, const evaluation, and diagnostics/lints.
- **Macros:** declarative expansion, procedural expansion, derives, built-in macros, hygiene, repetition, and expansion diagnostics.
- **Runtime and libraries:** Rust ABI and layout; panic, unwinding, and drop; `core`, `alloc`, and `std`; collections and iterators; I/O and operating-system APIs; networking, time, threads, synchronization, and atomics.
- **Cargo and tooling:** package and workspace selection; dependency resolution; target kinds; features and conditional compilation; build scripts; configuration, profiles, and environment; test/bench/doc workflows; compatible library artifacts and cross-crate metadata.
- **Platforms:** object formats, ABIs, linkers, sysroots, target-specific configuration, runtime startup, and cross-compilation on each required target.

## Status meanings

- **Available** means the exact narrow capability in that row has direct implementation evidence. It does not imply coverage of the enclosing subsystem.
- **Partial** means the row names an implemented subset and its remaining limits.
- **Not yet** means repository documentation or implementation evidence identifies the capability as unsupported.
- **Unassessed** means the repository does not contain enough evidence to classify it. Parsing alone is not implementation evidence.

The index deliberately retains unassessed rows instead of silently counting them as support or as a verified failure. As gaps are explored, replace an unassessed status with a precise supported subset or a documented unsupported behavior, and add differential cases for observable rules.

## Evidence and score boundaries

The Linux rustc comparison shown on the website is the pass count for the named, maintained cases only. Its denominator is the current case-file size; adding a useful regression case changes that denominator without changing the Rust language surface. Its numerator must never be described as an overall Rust compatibility percentage.

The website inventory is not a test denominator either. Rows group features for navigation and disclosure; adding rows improves visibility but does not make the compiler less compatible. A full compatibility claim requires the evidence listed in [`benchmarks/compatibility/TARGET.md`](../benchmarks/compatibility/TARGET.md), including upstream conformance suites and real crates on every required platform. No finite hand-written list proves equivalence for all Rust programs.

## Upstream maps

- [Rust Reference](https://doc.rust-lang.org/reference/) — language syntax, items, types, expressions, patterns, attributes, macros, and behavior.
- [Rust standard library](https://doc.rust-lang.org/std/) — stable `std` API; its crate documentation links to `core` and `alloc`.
- [Cargo Reference](https://doc.rust-lang.org/cargo/reference/) — manifests, targets, workspaces, dependency resolution, features, profiles, configuration, and build scripts.
