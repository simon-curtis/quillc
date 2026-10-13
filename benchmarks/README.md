# Benchmark protocol and experiment index

## Workload

`catalog-flow` is a checked-in, generated Rust workspace with eight packages
in a dependency DAG. One independent audit package supports incremental-edit
measurements. The fixture has about 31,000 source lines and no third-party
dependencies. Regenerate it with `python3 benchmarks/generate.py`; the script
reports the Rust line count.

This synthetic workload is a reproducible first gate, not a representative
sample of Rust crates. The paired Quill and `rustc` results are in
[`catalog-flow/RESULTS.md`](catalog-flow/RESULTS.md).

## Reproduce the Cargo baseline

Run on an otherwise idle machine:

```sh
python3 benchmarks/run.py --rounds 5
```

Each round clears the fixture target directory for the cold build, then runs
an unchanged warm build and an incremental build after touching one leaf
module. The script restores the source edit after each round. It does not drop
the OS page cache, so “cold” means a clean target directory, not a cold
machine or filesystem cache.

The runner records compiler and Cargo versions, CPU count, source revision,
wall time, process-tree CPU time, and cgroup v2 peak memory in
`benchmarks/results/baseline-<timestamp>.json`. Each invocation runs in its
own systemd user scope; CPU and peak-memory counters include child compiler
processes and the small Python wrapper. The script requires a working user
systemd manager and writable cgroup v2 accounting files, and fails when those
are unavailable.

Compare medians and ranges across repetitions. Keep the raw rows. A comparison
must hold source, compiler version, profile, target, linker, and machine
settings constant. Quill and `rustc` must produce behaviorally equivalent
binaries with equivalent optimization and debug settings.

## Acceptance gate

For every workload and build mode, require matching executable behavior,
`rustc median wall / Quill median wall >= 3.0`, and no higher Quill peak
memory. A frontend-only profile, a different artifact product, or this single
synthetic workload cannot establish the target or general Rust compatibility.
Compatibility also needs a separately curated Rust test suite.

## Current Cargo baseline

Five cold/warm/incremental cycles were measured on 2026-10-13 with
`python3 benchmarks/run.py --rounds 5` on a 30,977-line fixture. These are
`rustc`-only medians, not a paired acceptance result:

| Mode | Wall | CPU | Peak memory |
|---|---:|---:|---:|
| Cold | 2.646 s | 5.542 s | 324.6 MiB |
| Warm | 0.036 s | 0.038 s | 32.7 MiB |
| One-file incremental | 0.098 s | 0.108 s | 111.0 MiB |

Raw rows and host metadata are in
[`results/baseline-20261013-080835.json`](results/baseline-20261013-080835.json).
The paired Quill results, runtime comparison, and artifact-scope limits are in
[`catalog-flow/RESULTS.md`](catalog-flow/RESULTS.md).

## Frontend and Cargo profiles

These profiles measure specific frontend commands. They are not Cargo build
comparisons or evidence of Rust compatibility. Historical snapshots may use
different compiler states and measurement methods; compare only within each
report.

| Profile | Result | Raw measurements |
|---|---|---|
| `check-subset` diagnostic profile | 3,454 fixture bodies checked; parser AST-only path reduced median wall time 6.85% and peak RSS 26.07% versus its recorded before run. | [`quill-check-subset-20261012.json`](results/quill-check-subset-20261012.json), [`parser-ast-only-20261013.json`](results/parser-ast-only-20261013.json) |
| Shared parse/type-check pool | No measurable timing change; retained as a pool-lifecycle simplification. | [`shared-pool-20261013.json`](results/shared-pool-20261013.json) |
| Dead-body lowering | End-to-end change was within run spread; reduced backend work, with no established speedup. | [`dead-code-pruning-20261013.json`](results/dead-code-pruning-20261013.json) |
| Public-helper reachability | Stress case reduced peak RSS 14.6% and output size 99.5%; wall-time ranges overlapped. | [`dead-public-codegen-20261013-022048.json`](results/dead-public-codegen-20261013-022048.json) |
| Worker-count scaling | On the recorded frontend profile, 12 workers had the lowest wall median; 8 used less memory for a small wall-time increase. | [`parallel-scaling-20261013.json`](results/parallel-scaling-20261013.json) |
| Combined type-check traversal | Rejected: wall, CPU, and RSS medians slightly regressed, with overlapping ranges. | [`combined-typeck-walk-20261013.json`](results/combined-typeck-walk-20261013.json) |
| Indexed source-span lookup | Median wall time fell 63.86%, CPU 65.97%; peak RSS was effectively unchanged. This is frontend work reduction, not a compiler acceptance result. | [`indexed-span-lookup-20261013.json`](results/indexed-span-lookup-20261013.json) |
| Fused call collection | Rejected: benchmark output matched, but timing was within noise and wall/RSS slightly regressed. | [`fused-call-collection-20261013.json`](results/fused-call-collection-20261013.json) |
| Persistent query cache | Cold, warm, and one-file incremental checker profiles; cached parse/type-check results, not name resolution or codegen. | [`quill-query-cache-20261013-011506.json`](results/quill-query-cache-20261013-011506.json) |
| Struct-interface checker cache | Warm runs hit all entries; a one-file edit missed one parse/body entry. | [`quill-cargo-check-subset-cache-20261013-023814.json`](results/quill-cargo-check-subset-cache-20261013-023814.json) |
| Generic-inference checker sample | Slower and larger than the earlier profile, but compiler states differed; the effect was not isolated. | [`quill-cargo-check-subset-cache-20261013-065842.json`](results/quill-cargo-check-subset-cache-20261013-065842.json) |
| Cargo graph parse and HIR | Parsed 9 roots and 50 files; the later HIR version resolved 93 workspace calls. Neither profile type-checks or emits code. | [`quill-cargo-parse-cache-20261013-011917.json`](results/quill-cargo-parse-cache-20261013-011917.json), [`quill-cargo-parse-cache-20261013-012312.json`](results/quill-cargo-parse-cache-20261013-012312.json) |
| Cargo checker cache | Fixture bodies passed the supported primitive checks. Interface, `cfg`, and per-body cache snapshots are frontend measurements only. | [`quill-cargo-check-subset-cache-20261013-013113.json`](results/quill-cargo-check-subset-cache-20261013-013113.json), [`quill-cargo-check-subset-cache-20261013-014619.json`](results/quill-cargo-check-subset-cache-20261013-014619.json), [`quill-cargo-check-subset-cache-20261013-015622.json`](results/quill-cargo-check-subset-cache-20261013-015622.json) |
| Borrowed body selection | Small, mixed before/after changes; retained to avoid redundant AST ownership, without a claimed throughput gain. | [`quill-cargo-check-subset-cache-20261013-024749.json`](results/quill-cargo-check-subset-cache-20261013-024749.json), [`quill-cargo-check-subset-cache-20261013-024819.json`](results/quill-cargo-check-subset-cache-20261013-024819.json) |
| Parallel function-body checking | Higher worker caps showed no reliable wall-time advantage over four workers on the recorded checker runs. | [`parallel-typeck-20261013-050930.json`](results/parallel-typeck-20261013-050930.json), [`parallel-typeck-20261013-051130.json`](results/parallel-typeck-20261013-051130.json) |
| Shared frontend pool scaling | 12 workers had the lowest wall median; CPU and memory rose with worker count. | [`parallel-frontend-20261013-122322.json`](results/parallel-frontend-20261013-122322.json) |
| Parallel native helper lowering | Four workers improved the three-run wall median about 3.7%; ranges overlapped, so a reliable gain was not established. | [`parallel-codegen-20261013-062430.json`](results/parallel-codegen-20261013-062430.json) |

For current end-to-end measurements, use the paired report rather than these
isolated profiles.
