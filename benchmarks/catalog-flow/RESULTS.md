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
| `rustc cargo build -p catalog-flow` | Cold | 2.688 s | 5.603 s | 560.2 MiB |
| `quillc cargo-build-subset` | Cold | 0.325 s | 1.868 s | 134.9 MiB |
| `rustc cargo build -p catalog-flow` | Warm | 0.019 s | 0.020 s | 18.7 MiB |
| `quillc cargo-build-subset` | Warm | 0.007 s | 0.002 s | 18.3 MiB |
| `rustc cargo build -p catalog-flow` | Incremental | 0.289 s | 0.413 s | 222.0 MiB |
| `quillc cargo-build-subset` | Incremental | 0.027 s | 0.080 s | 35.4 MiB |
| `rustc cargo build -p catalog-flow` | Semantic edit | 0.305 s | 0.434 s | 229.7 MiB |
| `quillc cargo-build-subset` | Semantic edit | 0.081 s | 0.176 s | 52.4 MiB |

Quill's median wall-time ratios were 8.27× cold, 2.71× warm, 10.70× for the
comment edit, and 3.77× for the semantic edit. It used less sampled peak RSS
in all four states. The semantic-edit CPU-time ratio was 2.47× (0.176 s vs.
0.434 s). The warm wall-time ratio is below 3×, and this result measures only
the documented fixture; it does not establish general Rust compiler
performance.

The implementation work behind the latest run includes restricting the
cross-target function index to exports, memoizing repeated call resolution,
sharing a compile-wide type-check interface digest, parallel package-object
preparation, and reusing source-derived package objects. On an incremental
semantic edit, cached package objects now skip type-checking and HIR assembly;
only exported interfaces and generic bodies are retained for dependent crates.
A nested body-parallelism trial increased cold RSS without improving wall time
and was reverted. The latest change moves parsed function bodies into the
qualified-function index instead of deep-cloning them; on the semantic-edit
fixture, wall time fell from the previous 0.103 s median to 0.081 s, and peak
RSS fell from 73.5 MiB to 52.4 MiB. A three-run worker-count sweep found that
`QUILL_JOBS=4` used 0.122 s median CPU time at 0.086 s wall time, while the
12-worker setting used 0.172 s CPU at 0.081 s wall time. The current default
uses all available workers for the fastest observed wall time. These are
experiment notes, not separate acceptance results.

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
