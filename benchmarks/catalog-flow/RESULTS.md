# Catalog Flow: build time and binary runtime

## Reproduce

Build the compiler and run the paired measurement on an otherwise idle host.
The runner requires Python `psutil`.

```sh
cargo +nightly-2026-08-23 build --release -p quillc
python3 benchmarks/catalog-flow/measure.py
```

Retain [results.json](results.json): it records commands, compiler provenance,
raw samples, and medians. A successful run checks matching output, restores
source edits, and records each build mode and the generated-binary timings.
The tables below describe a recorded run; use the JSON for subsequent runs.

## Workload

The fixture has 30,992 Rust source lines in eight workspace crates. The
measured binary processes 128 records using generic `AsRef<str>` input, owned
strings, a vector, a `BTreeMap`, iterator validation, and aggregate returns.
Quill and `rustc` produced identical output:

```text
catalog-flow records=128 digest=0000a53352b1a3d7
```

## Paired build measurements

Five runs were collected on 2026-10-13 from a clean release build at revision
`44120ba` and Rust nightly `2026-08-23`, on an Intel i7-8700K host with 12
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
| `rustc cargo build -p catalog-flow` | Cold | 2.686 s | 5.699 s | 570.8 MiB |
| `quillc cargo-build-subset` | Cold | 0.333 s | 1.809 s | 133.1 MiB |
| `rustc cargo build -p catalog-flow` | Warm | 0.018969 s | 0.019616 s | 21.9 MiB |
| `quillc cargo-build-subset` | Warm | 0.001321 s | 0.001143 s | 21.9 MiB |
| `rustc cargo build -p catalog-flow` | Incremental | 0.287 s | 0.410 s | 227.7 MiB |
| `quillc cargo-build-subset` | Incremental | 0.027 s | 0.084 s | 35.8 MiB |
| `rustc cargo build -p catalog-flow` | Semantic edit | 0.308 s | 0.431 s | 227.9 MiB |
| `quillc cargo-build-subset` | Semantic edit | 0.083 s | 0.174 s | 52.1 MiB |

Quill's median wall-time ratios were 8.06× cold, 14.36× warm, 10.67× for the
comment edit, and 3.70× for the semantic edit. Peak RSS was lower in the cold
and both incremental states, and equal in the warm state. CPU-time ratios were
3.15× cold, 17.16× warm, 4.88× for the comment edit, and 2.47× for the semantic edit. This run
clears the 3× wall-time gate in all four states on this fixture, but semantic
CPU-time speedup remains below 3×. These results do not establish general Rust
compiler performance.

A separate clean semantic-incremental profile compared revisions `1301f37` and
`a364f73`: removing duplicate HIR clones reduced the `prepare combined HIR
input` phase from 30.72 ms to 12.86 ms (58%). The target compilation phase
fell from 86.51 ms to 69.77 ms in one profile sample. This phase result is
exploratory; it has no CPU or memory measurement and should not be confused
with the repeated end-to-end table above. See the [profile record](../results/quill-hir-preparation-20261013.json).

The warm cache-hit processes finish in about 1.35 ms for Quill. The former
2 ms psutil polling loop perturbed those short runs; direct alternating samples
measured a 1.24 ms Quill median, while the polling harness reported 6 ms. Warm runs now
use `wait4` directly for wall time and root-process peak RSS. The warm cache
path launches no compiler child processes; a separate observation saw no
children in 10 warmed Cargo invocations. Other build states retain sampled
process-tree RSS. The compiler source revision, binary hash, and raw samples are recorded in
[results.json](results.json). The previous sampler-based
run is retained in
[`results-20261013-010a3b9-polled.json`](results-20261013-010a3b9-polled.json)
and the intermediate millisecond-rounded run is in
[`results-20261013-56eeb4c-ms.json`](results-20261013-56eeb4c-ms.json),
and the earlier `3f16b03` run is retained in
[`results-20261013-3f16b03.json`](results-20261013-3f16b03.json).

## Generated binary runtime

A separate 2026-10-13 run at compiler revision `44120ba` timed the resulting
executables: five warmups followed by 25 alternating launches per binary.
Every invocation produced the same output. Wall time includes process startup.

| Binary | Median wall time | Range | Median CPU time |
|---|---:|---:|---:|
| `rustc` | 1.016 ms | 0.827–1.147 ms | 0.843 ms |
| Quill | 17.205 ms | 16.771–17.489 ms | 16.994 ms |

For this workload, the Quill binary's median wall time was 16.9× the `rustc`
binary's. These are debug-profile executables for one small synthetic fixture;
the measurements include launch overhead and do not establish runtime
performance for other programs. The raw per-launch samples are in
[results.json](results.json).
Slower generated code than LLVM-backed rustc is expected at this stage.
This runtime cost is separate from the build-time acceptance gate.

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
