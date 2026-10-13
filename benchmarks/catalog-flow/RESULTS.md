# `catalog-flow` results

The fixture has 30,992 Rust source lines in eight workspace crates. The
measured binary processes 128 records using generic `AsRef<str>` input, owned
strings, a vector, a `BTreeMap`, iterator validation, and aggregate returns.
Quill and `rustc` produced identical output:

```text
catalog-flow records=128 digest=0000a53352b1a3d7
```

## Paired build measurements

Five runs were collected on 2026-10-13 with a release Quill build and Rust
nightly `2026-08-23`, on an Intel i7-8700K host with 12 logical CPUs. Both
commands selected the same binary and eight-crate source dependency graph.
The table reports medians. Cold runs cleared the Rust target and Quill
parse/type-check/artifact caches, but retained the OS file cache. Warm runs
repeated an unchanged build. Comment-only incremental runs changed one source
file's comment. Semantic incremental runs changed the record range from 128 to
127, ran both executables, and checked that their changed outputs matched;
each edit was restored afterward.

Wall time uses a blocking process wait. CPU totals use kernel waited-child
accounting and exclude the RSS sampler. Peak RSS is the maximum sampled sum
for the build process and descendants.

| Compiler path | State | Wall | CPU | Peak process-tree RSS |
|---|---|---:|---:|---:|
| `rustc cargo build -p catalog-flow` | Cold | 2.658 s | 5.581 s | 565.9 MiB |
| `quillc cargo-build-subset` | Cold | 0.326 s | 1.891 s | 145.4 MiB |
| `rustc cargo build -p catalog-flow` | Warm | 0.019 s | 0.020 s | 18.9 MiB |
| `quillc cargo-build-subset` | Warm | 0.006 s | 0.002 s | 18.3 MiB |
| `rustc cargo build -p catalog-flow` | Incremental | 0.282 s | 0.407 s | 236.3 MiB |
| `quillc cargo-build-subset` | Incremental | 0.023 s | 0.077 s | 32.6 MiB |
| `rustc cargo build -p catalog-flow` | Semantic edit | 0.301 s | 0.425 s | 227.4 MiB |
| `quillc cargo-build-subset` | Semantic edit | 0.103 s | 0.190 s | 73.5 MiB |

Quill's median wall-time ratios were 8.15× cold, 3.17× warm, 12.26× for the
comment edit, and 2.92× for the semantic edit. It used less sampled peak RSS
in all four states. The semantic-edit result is below the 3× target.

The implementation work behind the latest run includes restricting the
cross-target function index to exports, memoizing repeated call resolution,
sharing a compile-wide type-check interface digest, parallel package-object
preparation, and reusing source-derived package objects. On an incremental
semantic edit, cached package objects now skip type-checking and HIR assembly;
only exported interfaces and generic bodies are retained for dependent crates.
A nested body-parallelism trial increased cold RSS without improving wall time
and was reverted. These are experiment notes, not separate acceptance
results.

## Scope

`cargo build -p catalog-flow` emits library artifacts and a binary. Quill
combines the supported source graph into one ELF executable; it does not emit
Cargo-compatible `.rlib` artifacts or implement general Rust. The source graph
and runtime workload match, but the output products differ. This result shows
native compilation and runtime parity for this fixture only.

Quill caches per-source parse ASTs under `QUILL_CACHE_DIR/parse-ast`, a
validated workspace plan under `QUILL_CACHE_DIR/cargo-workspaces`, and
source-derived package objects. The object key covers source and dependency
closure digests, target configuration, compiler stamp, and build environment.
Semantic edits rebuild the changed package object and relink the object set;
unchanged package objects bypass type-checking and HIR assembly. Source files
are still parsed and call resolution still runs for changed targets.

Quill emits x86-64 machine code and ELF itself; it has no LLVM or Cranelift
dependency. The `rustc` baseline uses the system nightly compiler.

Reproduce with:

```sh
cargo +nightly-2026-08-23 build --release -p quillc
python3 benchmarks/catalog-flow/measure.py
```

The measurement script requires Python `psutil`. Raw samples, medians, and
exact commands are in [results.json](results.json).
