# Work in this repository

Read the guide for the task before changing its implementation:

| Task | Start here |
| --- | --- |
| Compiler architecture or an end-to-end feature | [Architecture](docs/architecture.md) |
| Rust compatibility or feature status | [Completion target](benchmarks/compatibility/TARGET.md), then [corpus workflow](benchmarks/compatibility/README.md) |
| LSP behavior or latency | [Language server](docs/lsp.md) |
| Parser coverage or dependency upgrade | [Syntax contract](docs/rust-syntax-coverage.md) |
| Performance investigation or claim | [Benchmark protocol](benchmarks/README.md) |
| Website implementation or statistics | [Website workflow](docs/website/README.md) |
| Website design or review | [Visual direction](docs/website/design.md), then [review criteria](CODING_STANDARDS.md) |
| Arch installation or release | [Arch packaging](docs/arch-packaging.md) |

Keep documentation and website commits separate from compiler changes. Preserve
concurrent edits; stage named files or hunks rather than the whole checkout.
