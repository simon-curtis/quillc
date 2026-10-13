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

The latest clean candidate passes 33 of 43 cases (76.7%). It is isolated at
`5e51f46`: the last clean baseline `41a2eec` plus the signed `i32` widening
change from [`31da520`](https://github.com/simon-curtis/quillc/commit/31da520)
and tuple projections from
[`6563b91`](https://github.com/simon-curtis/quillc/commit/6563b91). The
code-generation source in this candidate is identical to those main-branch
commits. It resolves the signed-cast and tuple-field mismatches. The complete
report is
[`results/2026-10-13-edition-matrix-expanded-linux.json`](results/2026-10-13-edition-matrix-expanded-linux.json).

The report records the exact candidate source revision, compiler-source
cleanliness, binary hash, and build time. The compatibility page links to
[`TARGET.md`](TARGET.md), which defines the full destination: language,
diagnostics, standard library, Cargo, and the project's Linux and Windows
platforms. The 76.7% rate is only for these curated cases; it is not a general
Rust compatibility score and cannot be compared directly to the manually
curated 10.3% feature index. The earlier clean candidate at `5e51f46` passed
32 of 42 cases (76.2%).

Windows output remains unprobed. Ten cases fail: generic trait dispatch,
panic behavior, rustc's inability to consume Quill's object as an `--extern`
library, array-to-slice coercion, `Result` status checks, `Vec<String>`
indexing, `BTreeMap::get`, generic `AsRef<str>` dispatch, macro repetition,
and Rust 2015's valid `async` identifier. The paired Rust 2015/2018 tests show
that Quill rejects the identifier in both editions, while rustc accepts it in
2015 and rejects it in 2018. Four edition probes pass: Rust 2018 async syntax
parsing, Rust 2021 format capture, Rust 2024 if-let-chain parsing, and
experimental `future` edition async parsing. Loop break/continue, signed
widening, and tuple projections also match rustc. Build-script support covers
the tested native script that emits `rustc-cfg`; other directives and build
dependencies remain incomplete.

The latest main checkout still has uncommitted LSP wiring, and a clean build
of its newer driver commit fails because `quill_lsp` is absent from that
commit. The clean candidate above isolates the two committed code-generation
changes from that integration work; it does not validate those newer Cargo/LSP
changes or Windows output.
