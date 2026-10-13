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
`ed5eed4` and Rust nightly `2026-08-23`, on an Intel i7-8700K host with 12
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
| `rustc cargo build -p catalog-flow` | Cold | 2.682 s | 5.628 s | 571.7 MiB |
| `quillc cargo-build-subset` | Cold | 0.331 s | 1.823 s | 131.2 MiB |
| `rustc cargo build -p catalog-flow` | Warm | 0.018980 s | 0.019385 s | 22.0 MiB |
| `quillc cargo-build-subset` | Warm | 0.001313 s | 0.001125 s | 22.0 MiB |
| `rustc cargo build -p catalog-flow` | Incremental | 0.286 s | 0.412 s | 229.1 MiB |
| `quillc cargo-build-subset` | Incremental | 0.025 s | 0.083 s | 34.3 MiB |
| `rustc cargo build -p catalog-flow` | Semantic edit | 0.301 s | 0.427 s | 227.6 MiB |
| `quillc cargo-build-subset` | Semantic edit | 0.082 s | 0.175 s | 50.3 MiB |

Quill's median wall-time ratios were 8.11× cold, 14.46× warm, 11.36× for the
comment edit, and 3.65× for the semantic edit. Peak RSS was lower in the cold
and both incremental states, and equal in the warm state. CPU-time ratios were
3.09× cold, 17.23× warm, 4.95× for the comment edit, and 2.45× for the
semantic edit. This run clears the 3× wall-time gate in all four states on
this fixture, but semantic CPU-time speedup remains below 3×. These results
do not establish general Rust compiler performance. The previous clean run at
`798c9df` is retained in [`results-20261013-798c9df.json`](results-20261013-798c9df.json).

## Phase CPU attribution

One profiled semantic edit on compiler revision `8fabed9` measured source-graph
loading at 14.14 ms wall and 80.13 ms process CPU. The parse cache reported 49
hits and one miss; body type checking reported 506 cache hits and one miss.
Package-object preparation used 28.93 ms process CPU, type checking 24.64 ms,
and call resolution 16.26 ms. Process CPU includes worker threads, so it can
exceed wall time. This is an attribution sample, not a repeated speed result;
the [raw profile](../results/phase-cpu-20261013.json) records the method and
cache counters.

A nested call-resolution cache and borrowed qualified-call traversal were
measured against the same fixture. The candidate did not reduce the semantic
incremental medians: wall time rose 0.42% and CPU time rose 1.05%, with
overlapping sample ranges. One phase sample also showed no call-resolution
reduction, so the change was rejected. The exploratory cold wall-time result
was 6.25% lower, alongside 5.27% higher peak RSS; the raw comparison,
limitations, and [rejected patch](../experiments/call-resolution-cache.patch)
are recorded with the [experiment report](../results/call-resolution-cache-20261013.json).

A follow-up moved the combined struct table into target parses instead of
cloning it once. On the semantic edit, median wall time fell 1.96% while CPU
time rose 0.57%; all wall-time ranges overlapped. No stable CPU reduction was
measured, so this candidate was rejected too. The [patch](../experiments/struct-table-ownership.patch)
and [raw comparison](../results/struct-table-ownership-20261013.json) are retained.

A compact parse-summary cache keeps module declarations, item signatures, and
generic bodies while omitting non-generic function bodies for targets whose
package object is reusable. The clean five-round run at `db5f270`, after
rebasing onto parser recovery commit `d8465f4`, measured comment-only
incremental medians of 13.8 ms wall and 30.1 ms CPU, versus 25.0 ms and 83 ms
in the baseline. Semantic incremental CPU fell from 175 ms to 146 ms, while
wall time rose from 82.0 ms to 85.0 ms. Cold time was effectively unchanged
at 331 ms / 1.819 s CPU versus 331 ms / 1.823 s. Peak RSS remained below the
rustc baseline in all states, and semantic-edit output matched. An earlier
candidate at `3f0c376` measured 12.9 ms comment-only wall and 77.9 ms semantic
wall; the later runs show the semantic wall result is not a reliable
improvement. The measured benefit is comment-only incremental work; semantic
edits use less aggregate CPU with a small wall-time regression. Raw runs:
[`results-20261013-db5f270-parse-summaries.json`](results-20261013-db5f270-parse-summaries.json),
[`results-20261013-79e11e0-parse-summaries.json`](results-20261013-79e11e0-parse-summaries.json),
and [`results-20261013-3f0c376-parse-summaries.json`](results-20261013-3f0c376-parse-summaries.json).

The native object emitter previously cloned each target's complete parse tree
before appending external declarations. An ownership-taking path removes that
clone. At `9edab5c`, the semantic-edit medians moved from 85.0 ms / 146 ms CPU
to 80.1 ms / 131 ms CPU; the five-sample ranges overlap, so this remains an
inconclusive candidate. Comment-only incremental medians were 13.1 ms / 29.8
ms CPU, close to the prior 13.8 ms / 30.1 ms. Program output still matched.
The clean paired samples are in
[`results-20261013-9edab5c-owned-emission.json`](results-20261013-9edab5c-owned-emission.json).

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

A separate 2026-10-13 run at compiler revision `ed5eed4` timed the resulting
executables: five warmups followed by 25 alternating launches per binary.
Every invocation produced the same output. Wall time includes process startup.

| Binary | Median wall time | Range | Median CPU time |
|---|---:|---:|---:|
| `rustc` | 1.007 ms | see raw samples | 0.830 ms |
| Quill | 17.392 ms | see raw samples | 17.146 ms |

For this workload, the Quill binary's median wall time was 17.27× the `rustc`
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
