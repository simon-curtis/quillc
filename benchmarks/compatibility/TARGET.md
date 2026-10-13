# Rustc compatibility target

Quill's completion goal is to compile ordinary Rust crates with matching
language rules, diagnostics, and defined program behavior, including the
standard library and Cargo workflows, on Linux and Windows. Implementation
stages do not narrow this target. Use [the corpus workflow](README.md) to gather
evidence and [the inventory guide](../../docs/rust-compatibility-inventory.md)
to record gaps.

## Reference compiler and language modes

| Dimension | Required comparison |
| --- | --- |
| Compiler | Exact toolchain in [rust-toolchain.toml](../../rust-toolchain.toml), with version, target triple, and relevant flags recorded. |
| Stable editions | 2015, 2018, 2021, 2024. |
| Experimental edition | The pinned nightly's `future` mode with `-Zunstable-options`, evaluated separately from stable coverage. |
| Acceptance/rejection | Match type, trait, lifetime, borrow, macro, const, async, and unsafe rules for accepted and rejected input. |
| Diagnostics | Codes, messages, levels, spans, labels, and attached notes/help, normalizing temporary paths. |
| Defined runtime behavior | Exit status, stdout, stderr, panic behavior, and library behavior. |

Undefined behavior is outside runtime differential comparison. Compile-time
acceptance and diagnostics around such code still belong to the target.

## Platforms and Cargo

- **Linux:** `x86_64-unknown-linux-gnu` is the first required target.
- **Windows:** native x86-64 MSVC and GNU builds must be validated for
  `x86_64-pc-windows-msvc` and `x86_64-pc-windows-gnu` before a Windows claim.
- **Libraries:** cover the pinned toolchain's target-specific `core`, `alloc`,
  and `std` APIs.
- **Cargo:** cover `check`, `build`, `test`, and `run`; packages/workspaces;
  path, registry, and Git dependencies of every kind; features/`cfg`; build
  scripts; examples, tests, libraries, and binaries; profiles and target selection.
  Produce consumable library artifacts and preserve Rust ABI interoperability
  where rustc does.

## Evidence required for a 100% claim

Require all of the following:

1. No known gaps in the feature inventory or project specification.
2. No acceptance, diagnostic, or defined-runtime mismatches in the maintained
   differential suite across every required edition and platform/target pair.
3. Applicable upstream rustc language, UI/diagnostic, codegen, and Cargo suites
   pass, and real workspaces build and run with matching behavior on Linux and Windows.
4. The report names the Quill revision, rustc toolchain, targets, suites, and
   every exclusion with a reason. Untested required targets or unexplained
   exclusions prevent the claim.

The curated corpus is an early regression suite. Its pass rate and the
hand-curated feature index are diagnostic aids, not definitions of full
compatibility. A finite smoke suite cannot prove equivalence for every program.
