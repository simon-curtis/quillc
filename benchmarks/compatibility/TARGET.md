# Rustc compatibility target

This is Quill's completion target, taken from the project goal in
[`project.html`](../../project.html): compile ordinary Rust crates with the
same language rules, diagnostics, and program behavior as `rustc`, including
the standard library and Cargo workflows, on Linux and Windows. Compatibility
work may stage implementation, but no stage changes this destination.

## Reference compiler and language modes

- The reference compiler is the exact toolchain pinned by
  [`rust-toolchain.toml`](../../rust-toolchain.toml). Comparisons record its
  version, target triple, and relevant flags.
- Cover Rust editions 2015, 2018, 2021, and 2024. The pinned nightly also
  accepts the experimental `future` edition with `-Zunstable-options`; test
  that mode separately and do not count it as stable-edition coverage.
- Match both acceptance and rejection. Accepted programs must have matching
  type, trait, lifetime, borrow, macro, const, async, and unsafe semantics.
  Rustc diagnostics must match in code, message, level, source spans, labels,
  and attached notes/help after normalizing temporary paths.
- For programs whose behavior is defined by Rust, match observable exit status,
  stdout, stderr, panics, and standard-library behavior. Undefined behavior is
  outside differential runtime comparison; Quill must still match rustc's
  compile-time acceptance rules and diagnostics around it.

## Platforms and Cargo

- The first required target is Linux x86-64 (`x86_64-unknown-linux-gnu`).
- Windows output is also required. Validate native Windows x86-64 builds with
  the MSVC and GNU rustc targets (`x86_64-pc-windows-msvc` and
  `x86_64-pc-windows-gnu`) before claiming Windows compatibility.
- Cargo coverage includes `check`, `build`, `test`, and `run`; packages and
  workspaces; path, registry, and Git dependencies; all dependency kinds;
  features and `cfg`; build scripts; examples, tests, libraries, and binaries;
  profiles; and target selection. Quill must produce consumable Rust library
  artifacts and preserve Rust ABI interoperability where rustc does.
- The standard-library target is the API surface shipped with the pinned
  toolchain for each required target, including `core`, `alloc`, and `std`.

## Evidence required for a 100% claim

The hand-curated feature index and differential corpus are diagnostics, not a
percentage definition of full Rust compatibility. One passing case cannot
promote a broad feature to supported, and a finite smoke suite cannot prove
equivalence for every Rust program.

A 100% claim requires all of the following:

1. No known compatibility gaps in the feature index or project specification.
2. No acceptance, diagnostic, or defined-runtime mismatches across the
   maintained differential suite, with cases spanning every edition and
   required platform/target pair.
3. The applicable upstream rustc language, UI/diagnostic, codegen, and Cargo
   conformance suites pass, plus real-world Rust workspaces build and run with
   matching behavior on Linux and Windows.
4. The report identifies the exact Quill revision, rustc toolchain, target,
   test suites, and any excluded tests with reasons. An unexplained exclusion
   or untested required target prevents a 100% claim.

The current local corpus is an early differential suite. Run it with
[`README.md`](README.md); its pass rate is useful for regression tracking but
must not be presented as overall Rust compatibility.
