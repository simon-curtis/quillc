# `catalog-flow` results

The fixture has 30,992 Rust source lines in eight workspace crates. The
measured binary processes 128 records using generic `AsRef<str>` input, owned
strings, a vector, a `BTreeMap`, iterator validation, and aggregate returns.
Quill and `rustc` produced identical output:

```text
catalog-flow records=128 digest=0000a53352b1a3d7
```

## Paired build measurements

Five runs were collected on 2026-10-13 from a clean release build at revision
`56eeb4c` and Rust nightly `2026-08-23`, on an Intel i7-8700K host with 12
logical CPUs. Both commands selected the same binary and eight-crate source
dependency graph. The table reports medians. Cold runs cleared the Rust target
and Quill parse/type-check/artifact caches, but retained the OS file cache.
Warm runs repeated an unchanged build. Comment-only incremental runs changed
one source file's comment. Semantic incremental runs changed the record range
from 128 to 127, ran both executables, and checked that their changed outputs
matched; each edit was restored afterward.

Wall time uses a blocking process wait. CPU totals use kernel waited-child
accounting and exclude the RSS sampler. Peak RSS is the maximum sampled sum
for the build process and descendants.

| Compiler path | State | Wall | CPU | Peak process-tree RSS |
|---|---|---:|---:|---:|
| `rustc cargo build -p catalog-flow` | Cold | 2.698 s | 5.657 s | 568.3 MiB |
| `quillc cargo-build-subset` | Cold | 0.357 s | 1.901 s | 134.1 MiB |
| `rustc cargo build -p catalog-flow` | Warm | 0.019 s | 0.019 s | 21.6 MiB |
| `quillc cargo-build-subset` | Warm | 0.001 s | 0.001 s | 21.6 MiB |
| `rustc cargo build -p catalog-flow` | Incremental | 0.287 s | 0.410 s | 228.2 MiB |
| `quillc cargo-build-subset` | Incremental | 0.027 s | 0.085 s | 35.6 MiB |
| `rustc cargo build -p catalog-flow` | Semantic edit | 0.302 s | 0.428 s | 227.8 MiB |
| `quillc cargo-build-subset` | Semantic edit | 0.083 s | 0.178 s | 51.6 MiB |

Quill's median wall-time ratios were 7.56× cold, 19.00× warm, 10.63× for the
comment edit, and 3.64× for the semantic edit. Peak RSS was lower in three
states and equal in the warm state. CPU-time ratios were 2.98× cold, 19.00×
warm, 4.82× for the comment edit, and 2.40× for the semantic edit. This run
clears the 3× wall-time gate in all four states on this fixture, but cold and
semantic CPU-time ratios remain below 3×. These results do not establish
general Rust compiler performance.

The warm cache-hit processes finish in roughly 1 ms. The former 2 ms psutil
polling loop perturbed those short runs; direct alternating samples measured a
1.24 ms Quill median, while the polling harness reported 6 ms. Warm runs now
use `wait4` directly for wall time and root-process peak RSS. The warm cache
path launches no compiler child processes; a separate observation saw no
children in 10 warmed Cargo invocations. Other build states retain sampled
process-tree RSS. The clean compiler source revision is `56eeb4c`; its binary
hash and all raw samples are in `results.json`. The previous sampler-based
run is retained in
[`results-20261013-010a3b9-polled.json`](results-20261013-010a3b9-polled.json),
and the earlier `3f16b03` run is retained in
[`results-20261013-3f16b03.json`](results-20261013-3f16b03.json).

## Generated binary runtime

A separate 2026-10-13 run at compiler revision `56eeb4c` timed the resulting
executables: five warmups followed by 25 alternating launches per binary.
Every invocation produced the same output. Wall time includes process startup.
The compiler working tree had changes in `Cargo.lock` and the LSP crate; the
full state is recorded in `results.json`.

| Binary | Median wall time | Range | Median CPU time |
|---|---:|---:|---:|
| `rustc` | 1.001 ms | 0.865–1.153 ms | 0.841 ms |
| Quill | 16.954 ms | 16.592–17.239 ms | 16.742 ms |

For this workload, the Quill binary's median wall time was 16.9× the `rustc`
binary's. These are debug-profile executables for one small synthetic fixture;
the measurements include launch overhead and do not establish runtime
performance for other programs. The raw per-launch samples and compiler state
are in [results.json](results.json).

The implementation work behind the latest run includes restricting the
cross-target function index to exports, memoizing repeated call resolution,
sharing a compile-wide type-check interface digest, parallel package-object
preparation, and reusing source-derived package objects. On an incremental
semantic edit, cached package objects now skip type-checking and HIR assembly;
only exported interfaces and generic bodies are retained for dependent crates.
A nested body-parallelism trial increased cold RSS without improving wall time
and was reverted. Moving parsed function bodies into the qualified-function
index instead of deep-cloning them reduced the semantic-edit median from
0.103 s to 0.086 s. The raw JSON records the compiler revision, source-dirty
paths, and binary hash for this clean run.

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
