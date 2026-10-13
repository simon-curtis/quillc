# Real-crate probes

This first set probes five small, widely used crates from the local crates.io
registry. It checks whether Cargo target discovery and Quill parsing cover
real-world crate layouts, then attempts the current primitive checker. It is
not a paired build benchmark: `cargo check` runs full `rustc` checking, while
`quillc cargo-check-subset` implements only a documented subset.

| Crate | Version | Rust source lines | Quill parse | Quill primitive check | `cargo check --lib` |
|---|---:|---:|---|---|---|
| `itoa` | 1.0.18 | 488 | Pass; 2 files, 12 function items | Reject; checked 10 bodies | Pass |
| `memchr` | 2.8.3 | 15,824 | Pass; 27 files, 388 function items | Reject; checked 370 bodies | Pass |
| `base64` | 0.23.1 | 8,891 | Pass; 17 files, 140 function items | Reject; checked 131 bodies | Pass |
| `byteorder` | 1.5.0 | 5,567 | Pass; 2 files, 183 function items | Reject; checked 157 bodies | Pass |
| `fastrand` | 2.5.0 | 970 | Pass; 2 files, 51 function items | Reject; checked 51 bodies | Pass |

The parses establish that the compiler can discover and parse these selected
crate roots. The checker failures expose gaps in supported type checking,
including unresolved standard-library methods, pointer operations, casts,
indexing, and `Result` propagation. They do not indicate that these crates are
invalid Rust: the pinned `rustc` toolchain successfully checked every crate.

## Reproduction

Use the versions above and run from each extracted crate directory:

```sh
cargo +nightly-2026-08-23 check --offline --lib
quillc cargo-parse Cargo.toml
quillc cargo-check-subset Cargo.toml
```

The corresponding raw command outcomes and one-shot wall times are in
[`results.json`](results.json). Those times are retained as run metadata only;
they are single observations from different work, not medians or a performance
comparison.

## Provenance and limits

The probe used `rustc 1.100.0-nightly (c54751567 2026-08-22)` and the existing
`target/release/quillc` binary with SHA-256
`91242d1a3b65d2e1fa9da1238c96896d3bd1d004c40c08251968158d5370178e`. That
binary was last modified at 2026-10-13 17:45:48 UTC. Its source revision is
unknown. The checkout was changing during this run, and rebuilding the release
compiler failed because `quill-cfg` and `quill-syntax` had incompatible
attribute types. Therefore this report records the last available binary's
observed behavior; it does not certify the current working-tree sources.

The next useful benchmark gate is to rerun this set from a buildable, recorded
compiler revision, then add small runtime comparisons only for crate workloads
that pass end-to-end compilation.

## Checker follow-up

A later dev build added `std::io::Result<T>` and local one-argument `Result<T>`
alias handling, trait-bound-aware `Read::read_exact` / `Write::write_all`
signatures, and recursive compatibility for nested `Option` and `Result`
types. On `byteorder 1.5.0`, those changes removed the `read_exact`,
`write_all`, `?`, `Ok` inference, and nested Result mismatch diagnostics. The
crate still reports 93 diagnostics, mainly from slices, unsafe pointer casts,
`TryInto`, and byte conversion APIs.

The same dev build still rejects the other four crates. Their current counts
and compiler binary hash are recorded in the `follow_ups` section of the raw
JSON. These probes use fresh Quill caches and are a new snapshot; the original
one-shot timings above remain historical and are not directly comparable.

A subsequent dev build added tuple-struct positional fields and construction,
common integer methods, raw-pointer types and casts, and standard pointer/slice
methods. The same five probes still reject: diagnostics fell to 28 (`itoa`),
930 (`memchr`), 971 (`base64`), and 57 (`byteorder`); `fastrand` reported 103.
The compiler hash, source revision, and per-crate outcomes are in the latest
`follow_ups` entry in [`results.json`](results.json). This is type-checking
progress, not crate compatibility or a performance comparison.
