# Quill Check language server

## Connect an editor

Configure the editor's language-server command as:

```sh
quill-check
```

The server speaks LSP over stdin/stdout. Logs and fatal errors use stderr.
`quillc lsp` remains an equivalent entry point. Setup is complete when the
editor can open a Rust document and receive versioned syntax diagnostics.

## Supported behavior

The server handles document open/change/close, syntax diagnostics, document
symbols, completion, hover, and definition for open documents. Name lookup is
syntax-derived: it does not perform Rust scope resolution or type checking.
Project-wide indexing, Quill semantic diagnostics, and request cancellation
remain future work.

The editor parser is separate from the compiler frontend. Its lossless-tree,
edition-selection, macro-token, and malformed-buffer requirements live in
[the syntax contract](rust-syntax-coverage.md). Full syntax parsing does not
imply compiler semantic or code-generation support. The pinned parser dependency
is in [the LSP manifest](../crates/quill-lsp/Cargo.toml); workspace toolchain and
minimum Rust requirements are in the root manifests.

## Performance design

1. The protocol thread applies edits to live versioned text and line indexes.
   It selects between incoming messages and parse results, so completed work
   wakes it without a polling interval.
2. A coalescing worker retains the last tree per open URI, reparses the smallest
   changed span between previous and newest snapshots, and produces diagnostics.
   The protocol loop remains responsive during this work.
3. Diagnostics, symbols, definition, and hover use the current document version.
   During reparsing, completion may use the live prefix and last completed symbol
   index; it marks that provisional response incomplete so the editor refreshes it.
4. Superseded results and results for closed documents are dropped. Each open
   creates a new generation, preventing an old close/open cycle's result from
   attaching to a reopened file.

The next step is a bounded, priority-aware analysis pool with cancellation and
interactive-query priority over diagnostics. Reuse unaffected symbol and
scope/name regions; update workspace indexes from changed files. Keep whole
workspace scans outside the keystroke path.

## Latency targets

These are design targets for the generated workload, not measurements:

| Operation | Target |
| --- | --- |
| Warm completion, hover, definition | p95 below 10 ms |
| Document symbols; single edit to syntax diagnostics | p95 below 20 ms |
| Open to initial syntax diagnostics | Below 50 ms |
| Interactive request during a 20-edit burst | Below 10 ms |

A burst's total completion time includes all edits and is not a single-request
latency. Provisional completion must remain identifiable as incomplete.

## Measure and change the server

Use [the LSP benchmark procedure](../benchmarks/README.md#lsp-latency) for the
release command, workload, and raw report. That report owns the measured values;
compare each target above with its matching operation and percentile.

For parser or edition changes, follow [the syntax upgrade gate](rust-syntax-coverage.md#evidence-and-upgrade-gate).
A latency improvement is acceptable only with the versioning, stale-result,
source-preservation, and diagnostics contracts intact. Synthetic latency alone
establishes neither syntax coverage nor representative workspace performance.
