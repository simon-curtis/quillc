# Catalog Flow worker-cap sweep

Four paired five-round runs at source revision `af0c14b` varied `QUILL_JOBS`
on the same Intel i7-8700K host (12 logical CPUs), fixture, release binary,
and benchmark harness. Each raw report includes rustc comparison rows, memory,
source provenance, and output checks:

| Workers | Quill cold wall | Quill cold CPU | Quill semantic wall | Quill semantic CPU |
|---:|---:|---:|---:|---:|
| 1 | 0.984 s | 0.988 s | 0.114 s | 0.114 s |
| 2 | 0.546 s | 1.012 s | 0.093 s | 0.115 s |
| 4 | 0.368 s | 1.154 s | 0.087 s | 0.123 s |
| 8 | 0.321 s | 1.683 s | 0.082 s | 0.154 s |

Raw reports: [1 worker](catalog-flow-jobs1-20261013.json),
[2 workers](catalog-flow-jobs2-20261013.json),
[4 workers](catalog-flow-jobs4-20261013.json), and
[8 workers](catalog-flow-jobs8-20261013.json).

The one-worker setting reduced process CPU but substantially increased cold
and semantic wall time. Four workers balanced wall time and CPU across the
states, while eight was slightly faster for semantic wall time but used more
CPU. The compiler default is now capped at four workers; `QUILL_JOBS` and
`QUILL_TYPECK_JOBS` can raise or lower the cap. The subsequent uncapped-by-env
default run at revision `ec3bd03` produced a 0.379 s cold build and a 0.087 s
semantic rebuild, confirming the four-worker profile within the five-run
variation. This tuning is supported by one 12-logical-CPU host and one fixture;
other hardware and larger workspaces still need measurements.
