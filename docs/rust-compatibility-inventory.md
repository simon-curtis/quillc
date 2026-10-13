# Rust compatibility inventory

Use this guide when adding or changing rows in [the compatibility page](../compatibility.html).
Rows group subsystems and disclose gaps; they are not a test denominator or a
measure of the size of Rust. The authoritative destination and required
platforms are in [the completion target](../benchmarks/compatibility/TARGET.md).

## Update a row

1. Name the precise capability, including relevant editions, targets, and limits.
2. Link direct implementation or documentation evidence and choose a status below.
3. Add differential cases for observable rules through [the corpus workflow](../benchmarks/compatibility/README.md).
4. Keep the row's broad status separate from probe results. Completion means the
   capability and its remaining gaps are explicit; one passing case cannot
   establish the surrounding subsystem.

## Status meanings

| Status | Required meaning |
| --- | --- |
| Available | The exact narrow capability has direct implementation evidence. |
| Partial | An implemented subset is named alongside its remaining limits. |
| Not yet | Repository evidence identifies the capability as unsupported. |
| Unassessed | Evidence is insufficient to classify it; parsing alone is insufficient. |

Retain unassessed rows until evidence supports a precise classification. They
are neither established support nor verified failures.

## Reference scope

Inventory ordinary stable Rust across the required editions, the pinned
toolchain's `core`, `alloc`, and `std`, Cargo workflows, and required targets.
Unstable language/library features sit outside the stable target even when the
pinned compiler parses or exposes them. Record target-specific evidence on its
own target; Linux results cannot establish Windows behavior.

| Group | Surface to disclose |
| --- | --- |
| Language | Lexical forms, primitives, expressions/operators, control flow, patterns, items/modules, generics/types, references/unsafe operations, attributes, edition rules, assembly, foreign declarations. |
| Checking | Name resolution, inference, traits/coherence, method selection, borrows, lifetimes, const evaluation, diagnostics/lints. |
| Macros | Declarative/procedural expansion, derives, built-ins, hygiene, repetition, expansion diagnostics. |
| Runtime and libraries | ABI/layout, panic/unwinding/drop, `core`/`alloc`/`std`, collections/iterators, I/O/OS APIs, networking, time, threads, synchronization, atomics. |
| Cargo and tooling | Package/workspace selection, dependencies, targets, features/`cfg`, build scripts, configuration/profiles/environment, test/bench/doc workflows, library artifacts and metadata. |
| Platforms | Object formats, ABIs, linkers, sysroots, target configuration, startup, cross-compilation. |

## Evidence and score boundaries

The differential report counts maintained cases. Adding a regression changes
that denominator without changing Rust's language surface. Inventory rows count
groups; adding a row improves disclosure without making the compiler less
compatible. Neither count is an overall conformance percentage. Use [the full
claim criteria](../benchmarks/compatibility/TARGET.md#evidence-required-for-a-100-claim)
for a completion claim.

## Upstream maps

- [Rust Reference](https://doc.rust-lang.org/reference/): language rules and behavior.
- [Rust standard library](https://doc.rust-lang.org/std/): stable APIs, with links to `core` and `alloc`.
- [Cargo Reference](https://doc.rust-lang.org/cargo/reference/): manifests, targets, dependencies, features, profiles, configuration, and scripts.
