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
`13849a3` and Rust nightly `2026-08-23`, on an Intel i7-8700K host with 12
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
| `rustc cargo build -p catalog-flow` | Cold | 2.681 s | 5.648 s | 567.3 MiB |
| `quillc cargo-build-subset` | Cold | 0.362 s | 1.956 s | 133.5 MiB |
| `rustc cargo build -p catalog-flow` | Warm | 0.018759 s | 0.019225 s | 21.9 MiB |
| `quillc cargo-build-subset` | Warm | 0.001290 s | 0.001111 s | 21.9 MiB |
| `rustc cargo build -p catalog-flow` | Incremental | 0.285 s | 0.406 s | 227.4 MiB |
| `quillc cargo-build-subset` | Incremental | 0.025 s | 0.088 s | 35.7 MiB |
| `rustc cargo build -p catalog-flow` | Semantic edit | 0.305 s | 0.433 s | 233.5 MiB |
| `quillc cargo-build-subset` | Semantic edit | 0.084 s | 0.178 s | 51.9 MiB |

Quill's median wall-time ratios were 7.41× cold, 14.54× warm, 11.23× for the
comment edit, and 3.62× for the semantic edit. Peak RSS was lower in three
states and equal in the warm state. CPU-time ratios were 2.89× cold, 17.31×
warm, 4.61× for the comment edit, and 2.43× for the semantic edit. This run
clears the 3× wall-time gate in all four states on this fixture, but cold and
semantic CPU-time ratios remain below 3×. These results do not establish
general Rust compiler performance.

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

A separate 2026-10-13 run at compiler revision `13849a3` timed the resulting
executables: five warmups followed by 25 alternating launches per binary.
Every invocation produced the same output. Wall time includes process startup.

| Binary | Median wall time | Range | Median CPU time |
|---|---:|---:|---:|
| `rustc` | 1.018 ms | 0.831–1.271 ms | 0.844 ms |
| Quill | 17.165 ms | 16.662–17.530 ms | 16.964 ms |

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
