# Rust compatibility corpus

Use this corpus to compare Quill with the pinned rustc. Read [the completion
target](TARGET.md) before interpreting a pass rate, and [the inventory guide](../../docs/rust-compatibility-inventory.md)
when updating feature status.

## Run the corpus

1. Build the Quill executable you intend to measure. Set `QUILLC` to select it
   explicitly; otherwise the runner chooses the newer of `target/release/quillc`
   and `target/debug/quillc`. It does not build the compiler.
2. Run from the repository:

   ```sh
   python3 benchmarks/compatibility/run.py
   ```

3. Read the report's exact compiler state and case outcomes. A case passes only
   when the observed results match. Keep any mismatch tied to its case and target.
   Use the runner's `--help` for filtering and output options; `--keep` retains
   temporary sources and artifacts for investigation.

The runner reads `channel` from [rust-toolchain.toml](../../rust-toolchain.toml)
and passes it to every rustc/Cargo invocation. Cases run in temporary directories,
where relying on rustup's repository override would select the system default.

## Case format

[cases.json](cases.json) owns fixtures, expectations, edition/flag overrides,
and feature IDs.

| Kind | Comparison |
| --- | --- |
| `run`, `cargo-run` | Compile with both compilers; compare executable exit status, stdout, and stderr. Cargo cases may list multiple `binaries`, all of which are compared. |
| `parse` | Syntax acceptance. |
| `syntax-reject` | Both compilers reject the source. |
| `diagnostic` | Expected rustc error code, primary message, and primary source location. |

The panic case normalizes rustc’s process-specific main-thread ID in stderr.
Exit status, payload, source location, and the rest of the panic-hook output
remain part of the comparison.

Broader diagnostic equivalence includes more than these current primary-error
checks; see [the target](TARGET.md#reference-compiler-and-language-modes).

To extend coverage, add cases across syntax, checking, diagnostics, runtime,
libraries, Cargo, and required targets. Tag feature IDs so the report aggregates
probes while retaining the curated status. Passing a case does not promote its
whole category to fully supported.

## Initial Linux baseline

The [initial 7/11 report](results/2026-10-13-initial-linux.json) (63.6%) is
historical and invalid as a rustc comparison: it recorded the repository
version but ran outside the repository without explicitly pinning the toolchain.
Use explicitly pinned reports for comparisons.

## Recorded Linux evidence

The [move-flow run](results/2026-10-13-move-flow-clean.json) passes 89/89
curated cases at clean revision `f392c5f`. In addition to the conditional move
diagnostic, it checks a use after moving a non-`Copy` value in a `while` body;
Quill matches rustc's E0382 code, message, and primary location. This 100%
corpus result does not establish overall Rust compatibility.

The [conditional-move run](results/2026-10-13-if-move-clean.json) passes
88/88 curated cases at clean revision `95b2509`. Its new diagnostic probe
checks a use after moving a non-`Copy` value through one arm of an `if`
expression; Quill matches rustc's E0382 code, message, and primary location.
This 100% corpus result does not establish overall Rust compatibility.

The [if-expression short-circuit run](results/2026-10-13-ifexpr-short-circuit-clean.json)
passes 87/87 curated cases at clean revision `fc2049f`. It adds value-producing
`if` initializers whose skipped borrowed-slice index reads stay lazy and whose
reached out-of-bounds read matches rustc's panic output. This 100% corpus result
does not establish overall Rust compatibility.

The [slice short-circuit run](results/2026-10-13-slice-short-circuit-clean.json)
passes 85/85 cases at clean revision `ee8d02d`. It verifies skipped indexed
operands do not panic under `false &&` or `true ||`, reached operands do panic,
and a `while` length guard avoids indexing past the slice. This curated pass
rate is not an overall Rust compatibility percentage.

The [mutable-slice indexing run](results/2026-10-13-mut-slice-index-clean.json)
passes 82/82 cases at clean revision `fc69255`. It checks runtime indexing
through `&mut [u64]` and an out-of-bounds access with matching rustc panic
output. Other mutable-slice operations remain incomplete. This curated pass
rate is not an overall Rust compatibility percentage.

The [indexed-slice if-condition run](results/2026-10-13-slice-if-index-clean.json)
passes 80/80 cases at clean revision `8df05ff`. It checks both outcomes of an
`if` whose condition indexes a borrowed slice, plus a matching runtime
bounds-panic case. This curated pass rate is not an overall Rust compatibility
percentage.

The [indexed-slice while-condition run](results/2026-10-13-slice-while-index-clean.json)
passes 78/78 cases at clean revision `3ed33bb`. Its loop traverses a borrowed
slice by reevaluating a runtime index on each iteration; a second case runs
past the slice and matches rustc's bounds panic. This curated pass rate is not
an overall Rust compatibility percentage.

The [fixed-i32-array runtime-index run](results/2026-10-13-fixed-i32-array-index-clean.json)
passes 76/76 cases at clean revision `cac1ddc`. It adds in-range and
out-of-range runtime indexing for immutable `[i32; N]` arrays, matching
rustc's output and bounds-panic location. Other array element types and
mutation remain incomplete. This curated pass rate is not an overall Rust
compatibility percentage.

The [borrowed-slice runtime-index run](results/2026-10-13-borrowed-slice-index-clean.json)
passes 74/74 cases at clean revision `746950b`. It checks runtime `u64` and
`i32` indexing through borrowed slices plus an out-of-bounds `u64` access with
matching panic text and source location. Other slice element layouts and
indexing contexts remain incomplete. This curated pass rate is not an overall
Rust compatibility percentage.

The [fixed-array runtime-index run](results/2026-10-13-fixed-array-runtime-index-clean.json)
passes 71/71 cases at clean revision `39f0b91`. It adds runtime `u64`/`usize`
subscripts for selected immutable integer arrays and checks both a successful
lookup and rustc-matching bounds-panic text and location. Other array types,
mutation, and indexing contexts remain incomplete. This curated pass rate is
not an overall Rust compatibility percentage.

The [Vec mutable-assignment run](results/2026-10-13-vec-index-assignment-clean.json)
passes 69/69 cases at clean revision `5ad3860`. It adds in-range and
out-of-range scalar Vec indexing in mutable assignment expressions; the latter
matches rustc's bounds-panic output. This curated pass rate is not an overall
Rust compatibility percentage.

The [Vec explicit-return run](results/2026-10-13-vec-index-returns-clean.json)
passes 67/67 cases at clean revision `99b3c77`. It adds in-range and
out-of-range scalar Vec indexing in explicit return expressions; the latter
matches rustc's bounds-panic output. This is curated regression coverage, not
an overall Rust compatibility percentage.

The [Vec scalar-index run](results/2026-10-13-vec-u64-index.json) adds
rustc differential cases for `Vec<u64>` growth beyond its initial capacity,
`Vec<i32>` and `Vec<u64>` local indexing, and integer iteration. Quill passes
these behaviors for an in-range index used to initialize a local; the report
does not cover bounds panics or other indexing forms. The full pinned corpus
passes 60/60 cases (100.0%) at its recorded source revision. This is regression
coverage, not an overall Rust compatibility percentage.

The [Vec bounds-panic run](results/2026-10-13-vec-index-bounds-clean.json)
adds an out-of-range `Vec<u64>` access and matches rustc's exit status, stderr,
panic location, index, and length. It tests the supported local-initializer
indexing path; general `Index` compatibility remains incomplete. The combined
pinned corpus passes 61/61 cases (100.0%) at the report's recorded source
revision; this remains regression coverage rather than an overall compatibility
percentage.

The [Vec expression-index run](results/2026-10-13-vec-index-expressions-clean.json)
adds indexing inside binary arithmetic and function arguments, including an
out-of-range index nested in a call. It checks both successful values and the
same bounds-panic output. The combined pinned corpus passes 63/63 cases
(100.0%) at its recorded source revision; general indexing contexts remain
incomplete.

The [Vec if-branch run](results/2026-10-13-vec-index-if-branches-clean.json)
adds scalar indexing inside selected conditional-expression branches. It
checks that an unchosen out-of-range branch does not panic and that a chosen
out-of-range branch matches rustc's panic output. The combined pinned corpus
passes 65/65 cases (100.0%) at its recorded source revision; this remains
curated regression evidence rather than an overall compatibility percentage.

The [self-contained rlib interoperability run](results/2026-10-13-rlib-extern-clean.json)
passes all 58/58 curated cases at clean revision `e3f5db6`. Its artifact case
builds a Quill object, packages it with rustc-generated metadata, and verifies
that pinned rustc accepts the `.rlib` through `--extern` and produces matching
runtime output for `i32`, `u64`, and `usize` public functions. The rlib path is
limited to safe, synchronous, non-generic, top-level scalar signatures on Linux
x86-64. This 100% corpus result does not establish overall Rust compatibility.

The [C string literal stringify run](results/2026-10-13-stringify-c-literals-clean.json)
passes 57/58 cases (98.3%) at clean revision `10d029e`. The runtime
`stringify!` comparison now includes a C string literal and matches rustc;
source spans preserve token spelling without re-lexing literal forms. The one
remaining failure is rustc rejecting Quill's `.o` library artifact through
`--extern`. This corpus result is a regression measurement, not an overall
compatibility percentage.

The [panic-runtime clean run](results/2026-10-13-panic-runtime-clean.json)
at revision `26b9230` records 51/53 passing cases (96.2%), with binary hash and
clean source status. It is the earlier baseline snapshot, not the current count.

The [clean stringify spacing run](results/2026-10-13-stringify-spacing-clean.json)
passes 57/58 cases (98.3%) at clean revision `429971b`. Its `stringify!`
comparison covers nested delimiters, punctuation spacing, comments, and paths;
the implementation uses original token spans to preserve Rust's spacing rules.
The one failure is rustc consuming a Quill object through `--extern`. The report
includes the toolchain, binary hash, and exact case outcomes.

The earlier [clean stringify run](results/2026-10-13-stringify-clean.json) at
`ae21997` tested a simple arithmetic-token form. It also passed 57/58 cases,
with the same `.o`/`--extern` mismatch.

The earlier [Cargo feature selection run](results/2026-10-13-cargo-feature-selection.json)
passes 56/57 cases (98.2%). It verifies selected features through both
`--no-default-features --features=slow` and `--all-features` during builds, a
feature-selected type check, and `env!` expansion against the compiler's
environment. Its remaining failure is rustc consuming a Quill object through
`--extern`. This report is evidence for its named cases and corpus snapshot.

The [clean post-index run](results/2026-10-13-clean-40834d9.json) passes 54/55
cases at clean revision `40834d9`, with a clean compiler source tree and binary
hash. Its only failure remains the `.o` artifact passed through `rustc --extern`.

The [latest clean run](results/2026-10-13-clean-1301f37.json) passes 56/57
cases at revision `1301f37` and includes the compile-time `env!` compatibility
case. The same `.o`/`--extern` interop case is the only failure.

The [clean HIR-copy run](results/2026-10-13-clean-a364f73.json) also passes
56/57 cases at `a364f73`; the only failure remains `.o`/`--extern`
interoperability.

The [streamed-fingerprint run](results/2026-10-13-clean-44120ba.json) passes
57/58 cases at clean revision `44120ba`, including the compile-time
`stringify!` case. Only `.o`/`--extern` interoperability remains failing.

The earlier [generic trait dispatch run](results/2026-10-13-generic-trait-dispatch.json)
passes 52/53 cases (98.1%) and verifies one simple local trait method dispatched
through a generic bound to a concrete named struct. It records a modified
checkout at source revision `adda254`.

It includes 13 edition-scoped cases across 2015, 2018, 2021, 2024, and
experimental `future`: Rust 2015 uses of `async`, `await`, and `dyn`; later
reserved-keyword behavior; Rust 2024 `gen`/`r#gen`, unsafe extern blocks, and
unsafe attributes. Maintained cases also cover loop break/continue, signed
widening, tuple projections, borrowed integer-slice iteration,
`Vec<String>` growth/indexing/cloning/output, and a `Result` status check.
Static-string panic behavior also matches in the maintained runtime case.
The native build-script case emits `rustc-cfg`; other directives and build
dependencies remain incomplete.

The earlier snapshot failed both generic trait dispatch and rustc consuming
Quill's object as an `--extern` library. Windows remains unprobed.
New reports retain their own results and provenance under `results/`; the website
selects reports through [its data loader](../../website/data.mjs).

The 96.2% figure describes those 53 cases only. The [full compatibility target](TARGET.md)
requires broader suites, real workspaces, and every required platform. Neither
case counts nor grouped inventory counts are overall Rust percentages.
