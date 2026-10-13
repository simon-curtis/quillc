# Rust compatibility corpus

This directory holds executable comparisons against the Rust compiler named by
`rust-toolchain.toml`. It is the evidence source for compatibility work; the
website compatibility inventory groups the Rust surface by subsystem; it is
not a test denominator or a conformance score. See
[the inventory scope](../../docs/rust-compatibility-inventory.md).

The completion target and evidence required before claiming full compatibility
are defined in [`TARGET.md`](TARGET.md). The corpus below is an early
regression suite, not a proxy percentage for that full target.

The runner reads the pinned `channel` and passes it explicitly to every
`rustc` and `cargo` command. Cases run in temporary directories, so relying on
rustup's directory-based toolchain override would otherwise silently select
the machine's default compiler instead.

Run the corpus with:

```sh
python3 benchmarks/compatibility/run.py
```

The runner builds each positive case with both compilers, runs both binaries,
and compares exit status, stdout, and stderr. A Cargo case can list multiple
`binaries`; the runner compares the output of every generated target.
Diagnostic cases compare the expected rustc error code, primary message, and
primary source location recorded in `cases.json`. Passing one diagnostic case
does not establish broad diagnostic or borrow-checker compatibility.

The report includes the exact `rustc` version, Quill binary, case results, and
overall pass rate. A case passes only when the observed rustc and Quill results
match. Never use a small smoke-corpus percentage as a substitute for the
full-language goal: add cases across syntax, type and borrow rules, diagnostics,
runtime and standard-library behavior, Cargo workflows, and supported targets.
Each case also names its compatibility-index feature IDs. The report aggregates
those probes by feature while preserving the hand-curated status; a passing
probe does not promote a broad feature category to full compatibility.
The panic runtime case normalizes rustc's process-specific main-thread ID in
stderr; it still compares the exit status, payload, source location, and
remaining panic-hook output.

## Case format

`cases.json` contains source or Cargo workspace fixtures. `run` and `cargo-run`
cases compare runtime behavior, `parse` cases compare syntax acceptance,
`syntax-reject` cases require both compilers to reject source, and `diagnostic`
cases compare error codes, messages, and primary spans. Each case
can override edition and rustc flags. Temporary artifacts are removed after
the run unless `--keep` is supplied.

`QUILLC` may point to a built Quill executable. By default the runner selects
the most recently built `target/release/quillc` or `target/debug/quillc`; it
does not build the compiler itself.

## Initial Linux baseline

On 13 October 2026, the initial report showed 7 of 11 cases passing (63.6%).
It predates explicit toolchain pinning: it recorded the repository rustc
version, but ran cases outside the repository where rustup selected the system
default. Do not treat it as a valid rustc comparison; the corrected current
report below supersedes it. The saved historical report is
[`results/2026-10-13-initial-linux.json`](results/2026-10-13-initial-linux.json).

The latest clean Linux run passes 51 of 53 cases (96.2%) at revision
`26b9230`. The report records the exact binary hash and clean source status.
It covers static-string panic behavior, Rust 2015 `async`, `await`, and `dyn`
identifiers, and Rust 2024 reserved `gen`, unsafe extern block, and unsafe
attribute rules:
[`results/2026-10-13-panic-runtime-clean.json`](results/2026-10-13-panic-runtime-clean.json).
Borrowed integer slice iteration and `Vec<String>` growth, indexing, cloning,
and output match rustc in the maintained cases. Windows output remains
unprobed.

Two cases fail: generic trait dispatch and rustc's inability to consume
Quill's object as an `--extern` library. The paired Rust 2015/2018
tests now match rustc: `async`, `await`, and `dyn` are accepted as Rust 2015
identifiers where appropriate, while Rust 2018 reserves `async` for async
syntax. Thirteen edition-scoped cases pass across Rust 2015, 2018, 2021, 2024,
and experimental `future`, including Rust 2024 `r#gen` and required unsafe
syntax for extern blocks and attributes. Loop break/continue,
signed widening, tuple projections, borrowed slice iteration, `Vec<String>`
indexing, and the tested `Result` status check also match rustc. Build-script support covers the tested native script that emits
`rustc-cfg`; other directives and build dependencies remain incomplete.

The 96.2% rate is only for these curated cases; it is not an overall Rust
compatibility score. The case count and the grouped website inventory describe
different things and must not be compared as percentages. The full destination
remains in [`TARGET.md`](TARGET.md),
including language, diagnostics, standard library, Cargo, and Linux and
Windows platforms. Earlier reports remain available for historical runs.
