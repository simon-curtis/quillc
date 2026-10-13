# Rust compatibility corpus

This directory holds executable comparisons against the Rust compiler named by
`rust-toolchain.toml`. It is the evidence source for compatibility work; the
website's 29 manually categorized entries remain an implementation index, not
a conformance score.

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

## Case format

`cases.json` contains source or Cargo workspace fixtures. `run` and `cargo-run`
cases compare runtime behavior, `parse` cases compare syntax acceptance, and
`diagnostic` cases compare error codes, messages, and primary spans. Each case
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

The latest clean baseline run passes 30 of 42 cases (71.4%) against Quill
revision `41a2eec` and probes 28 of 29 tracked categories. The 14 added cases
exercise variations in integer conversion, tuples, slices, `Result`, collections,
generic dispatch, macro repetition, and Rust editions. Five of those 14 match
rustc: loop break/continue, Rust 2018 async syntax parsing, Rust 2021 format
capture, Rust 2024 if-let-chain parsing, and experimental `future` edition
async parsing.

The report at
[`results/2026-10-13-current-linux.json`](results/2026-10-13-current-linux.json)
records the exact source revision, compiler-source cleanliness, binary hash,
and build time. The compatibility page links to [`TARGET.md`](TARGET.md), which
defines the full destination: language, diagnostics, standard library, Cargo,
and the project's Linux and Windows platforms. The 71.4% rate is only for
these curated cases; it is not a general Rust compatibility score and cannot
be compared directly to the manually curated 10.3% feature index.

Windows output remains unprobed. Twelve cases fail: generic trait dispatch,
panic behavior, rustc's inability to consume Quill's object as an `--extern`
library, signed integer casts, tuple field access, array-to-slice coercion,
`Result` status checks, `Vec<String>` indexing, `BTreeMap::get`, generic
`AsRef<str>` dispatch, macro repetition, and Rust 2015's valid `async`
identifier. Build-script support currently covers the tested native script
that emits `rustc-cfg`; other directives and build dependencies remain
incomplete.

The clean baseline is revision `41a2eec`. A clean checkout of newer commit
`456aabc` failed to build because its driver references `quill_lsp`, which is
not present in that committed workspace. The shared working tree contains
ongoing compiler and LSP edits; reports from it must remain marked dirty until
those changes are committed and rebuilt from a clean checkout.
