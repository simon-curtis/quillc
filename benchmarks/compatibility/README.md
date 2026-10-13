# Rust compatibility corpus

This directory holds executable comparisons against the Rust compiler named by
`rust-toolchain.toml`. It is the evidence source for compatibility work; the
website's 29 manually categorized entries remain an implementation index, not
a conformance score.

Run the corpus with:

```sh
python3 benchmarks/compatibility/run.py
```

The runner builds each positive case with both compilers, runs both binaries,
and compares exit status, stdout, and stderr. Diagnostic compatibility needs
a separate, versioned expectation set and is not claimed by this first
executable tier.

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

On 13 October 2026, the initial 11-case executable set passed 7 cases (63.6%)
against `rustc 1.100.0-nightly` on x86-64 Linux. The saved machine-readable
report is [`results/2026-10-13-initial-linux.json`](results/2026-10-13-initial-linux.json).
The four observed mismatches are two loop cases whose computed values are
rejected by the `println!` capture type gate, a struct field initializer, and
fixed-array indexing. This small initial set is only a reproducible starting
point; the full compatibility target remains all stable Rust behavior, Rust
diagnostics, Cargo workflows, and Linux and Windows targets described in the
project specification.

The current 28-case run passes 17 (60.7%) against clean Quill revision `cdbcbb6` and
probes 28 of 29 tracked categories. The local workspace dependency and feature
cfg cases match rustc. A narrow two-`u64` Rust ABI call also links from a
Quill object into a rustc host and matches output. The report at
[`results/2026-10-13-current-linux.json`](results/2026-10-13-current-linux.json)
records the exact source revision, compiler-source cleanliness, binary hash, and
build time. The website reports this result separately from the manually curated
10.3% documentation index.

Windows output is the only tracked category with no probe. Eleven cases fail,
covering multiple binary targets, build scripts, diagnostic parity for type
mismatch and use-after-move, Option status checks, owned strings, derive
implementations, trait dispatch, Vec iteration, panic behavior, and rustc's
inability to consume Quill's object as an `--extern` library. Macro syntax and
one literal macro expansion now pass. The 60.7% figure applies only to these
28 curated cases; it is not a general Rust compatibility score and cannot be
compared directly to the manual 10.3% index ratio.
