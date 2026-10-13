# Rust compatibility corpus

This directory holds executable comparisons against the Rust compiler named by
`rust-toolchain.toml`. It is the evidence source for compatibility work; the
website's 29 manually categorized entries remain an implementation index, not
a conformance score.

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

The current 28-case run passes 24 (85.7%) against clean Quill revision
`b8308f5` and probes 28 of 29 tracked categories. The local workspace dependency,
feature cfg, multiple binary targets, scalar struct clone/equality, and narrow
unsigned-integer Vec push/iteration cases match rustc. A narrow two-`u64` Rust
ABI call also links from a Quill object into a rustc host and matches output.
The report at
[`results/2026-10-13-current-linux.json`](results/2026-10-13-current-linux.json)
records the exact source revision, compiler-source cleanliness, binary hash,
and build time. The website reports this result separately from the manually curated
10.3% documentation index. The full compatibility target remains stable Rust
behavior, Rust diagnostics, Cargo workflows, and Linux and Windows targets as
described in the project specification.

Windows output is the only tracked category with no probe. Four cases fail,
covering build scripts, generic trait dispatch, panic behavior, and rustc's
inability to consume Quill's object as an `--extern` library. The use-after-move
diagnostic now matches rustc for the tested `String` formatting capture. Macro
syntax and one literal macro expansion also pass. The 85.7% figure applies only to these
28 curated cases; it is not a general Rust compatibility score and cannot be
compared directly to the manual 10.3% index ratio.
